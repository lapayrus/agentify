#!/usr/bin/python3 -u
"""agentify-exec — run one command in a Project's distro, cancellably.

The daemon runs this as

    wsl.exe -d <distro> -- /usr/local/bin/agentify-exec \\
        --marker <marker> --workdir <dir> [--cwd <dir>] [--env K=V]... -- <argv...>

and keeps the launcher's two pipes: stdout and stderr are the command's own, kept
apart, and the launcher's exit status is the command's exit status.

Two things this wrapper exists for, both invisible to the command:

* **A process group of its own.** The command becomes the leader of a fresh
  process group, and its group id is written to `<workdir>/<marker>.pgid`, so the
  daemon can terminate the whole group — not just the process it started — when
  the call is cancelled or times out.
* **A marker in the environment.** `AGENTIFY_EXEC_MARKER=<marker>` is inherited
  by everything the command starts, which is what lets the cancel path find a
  process that escaped its group, by reading `/proc/<pid>/environ`.

Standard input is whatever the launcher was given: the daemon writes a bounded
body and closes the pipe. There is no PTY in v0.
"""

import os
import sys


def die(message):
    sys.stderr.write("agentify-exec: %s\n" % message)
    sys.exit(127)


def parse(argv):
    marker = None
    workdir = "/run/agentify/exec"
    cwd = None
    environment = []
    index = 0
    while index < len(argv):
        token = argv[index]
        if token == "--":
            index += 1
            break
        if token in ("--marker", "--workdir", "--cwd", "--env"):
            if index + 1 >= len(argv):
                die("%s needs a value" % token)
            value = argv[index + 1]
            if token == "--marker":
                marker = value
            elif token == "--workdir":
                workdir = value
            elif token == "--cwd":
                cwd = value
            else:
                environment.append(value)
            index += 2
            continue
        die("unknown argument %r" % token)
    command = argv[index:]
    if not marker:
        die("--marker is required")
    if not command:
        die("no command to run")
    return marker, workdir, cwd, environment, command


def main():
    marker, workdir, cwd, environment, command = parse(sys.argv[1:])

    try:
        os.makedirs(workdir, exist_ok=True)
    except OSError:
        pass

    # A fresh process group, so a cancelled call takes every child with it. If
    # the process is already a group leader (some init systems make it one) this
    # raises, and the daemon's marker sweep still finds the command.
    try:
        os.setpgid(0, 0)
    except OSError:
        pass

    try:
        with open(os.path.join(workdir, marker + ".pgid"), "w") as handle:
            handle.write(str(os.getpid()))
    except OSError:
        pass

    os.environ["AGENTIFY_EXEC_MARKER"] = marker
    for pair in environment:
        key, separator, value = pair.partition("=")
        if not separator or not key:
            die("--env needs KEY=VALUE, not %r" % pair)
        os.environ[key] = value

    if cwd:
        try:
            os.chdir(cwd)
        except OSError as error:
            die("cannot enter %s: %s" % (cwd, error.strerror))

    try:
        os.execvp(command[0], command)
    except OSError as error:
        die("%s: %s" % (command[0], error.strerror))


if __name__ == "__main__":
    main()
