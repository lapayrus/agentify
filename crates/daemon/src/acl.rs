//! File permission handling for the discovery file.
//!
//! The discovery file carries a bearer token, so it must be readable by the
//! OS user who runs the daemon and by nobody else. On Windows the data root
//! already sits in the user profile (inherited ACLs are user-only); we still
//! replace the DACL with a single explicit ACE for the current user, so the
//! guarantee does not depend on where `AGENTIFY_HOME` points.

use std::io;
use std::path::Path;

pub use platform::{current_user, grantees, restrict_to_current_user};

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID,
        TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        GetAce, GetTokenInformation, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, PSECURITY_DESCRIPTOR, PSID,
        TOKEN_QUERY, TOKEN_USER, TokenUser, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// `FILE_ALL_ACCESS`: read, write, execute, and delete.
    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

    /// `ACCESS_ALLOWED_ACE_TYPE` from `Win32::System::SystemServices`.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    /// The current process user's SID, as `S-1-5-21-...`.
    pub fn current_user() -> io::Result<String> {
        with_current_user_sid(|sid| sid_to_string(sid))
    }

    /// SIDs that an allow ACE in the file's DACL grants access to.
    pub fn grantees(path: &Path) -> io::Result<Vec<String>> {
        unsafe {
            let name = wide(path);
            let mut dacl: *mut ACL = ptr::null_mut();
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let code = GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            );
            if code != ERROR_SUCCESS {
                return Err(win32_error(code, "GetNamedSecurityInfoW"));
            }

            let mut grantees = Vec::new();
            if !descriptor.is_null() {
                if dacl.is_null() {
                    // A null DACL grants everyone full control.
                    grantees.push("NULL_DACL".to_string());
                } else {
                    let count = (*dacl).AceCount;
                    for index in 0..u32::from(count) {
                        let mut ace: *mut c_void = ptr::null_mut();
                        if GetAce(dacl, index, &mut ace) == 0 {
                            continue;
                        }
                        let header = &*(ace as *const ACE_HEADER);
                        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                            continue;
                        }
                        let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
                        let sid = ptr::addr_of!(allowed.SidStart) as PSID;
                        grantees.push(sid_to_string(sid)?);
                    }
                }
                LocalFree(descriptor as HLOCAL);
            }
            Ok(grantees)
        }
    }

    /// Replace the file's DACL with one protected ACE granting the current
    /// user full control. No inherited or group grants survive.
    pub fn restrict_to_current_user(path: &Path) -> io::Result<()> {
        with_current_user_sid(|sid| unsafe {
            let entry = EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_ALL_ACCESS,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: NO_INHERITANCE,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_USER,
                    ptstrName: sid as *mut u16,
                },
            };
            let mut acl: *mut ACL = ptr::null_mut();
            let code = SetEntriesInAclW(1, &entry, ptr::null_mut(), &mut acl);
            if code != ERROR_SUCCESS {
                return Err(win32_error(code, "SetEntriesInAclW"));
            }

            let name = wide(path);
            let code = SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null_mut(),
            );
            LocalFree(acl as HLOCAL);
            if code != ERROR_SUCCESS {
                return Err(win32_error(code, "SetNamedSecurityInfoW"));
            }
            Ok(())
        })
    }

    fn with_current_user_sid<T>(f: impl FnOnce(PSID) -> io::Result<T>) -> io::Result<T> {
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut length: u32 = 0;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length);
            if length == 0 {
                let err = io::Error::last_os_error();
                CloseHandle(token);
                return Err(err);
            }
            let mut buffer = vec![0u8; length as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut c_void,
                length,
                &mut length,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*(buffer.as_ptr() as *const TOKEN_USER);
            f(user.User.Sid)
        }
    }

    fn sid_to_string(sid: PSID) -> io::Result<String> {
        unsafe {
            let mut raw: *mut u16 = ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut raw) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0usize;
            while *raw.add(len) != 0 {
                len += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(raw, len));
            LocalFree(raw as HLOCAL);
            Ok(value)
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn win32_error(code: u32, call: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::Other,
            format!("{call} failed with Win32 error {code}"),
        )
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    pub fn current_user() -> io::Result<String> {
        Ok(format!("uid:{}", unsafe { libc::geteuid() }))
    }

    pub fn grantees(path: &Path) -> io::Result<Vec<String>> {
        let metadata = std::fs::metadata(path)?;
        Ok(vec![format!("uid:{}", metadata.uid())])
    }

    pub fn restrict_to_current_user(path: &Path) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
}

/// Convenience: the file's grantees, or an error when the platform cannot
/// report them.
pub fn assert_user_only(path: &Path) -> io::Result<bool> {
    Ok(grantees(path)? == vec![current_user()?])
}
