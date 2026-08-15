# shellcheck shell=bash
#
# Reap only the processes a test actually started.
#
# The e2e recipes used to end their cleanup traps with `pkill -x etrs`, which kills
# EVERY etrs the invoking user owns -- including the server behind a live remote
# session on the machine running the tests. That is the same defect v0.7.7 fixed in
# `just clean`, and it is worse here: running a test suite is not a request to drop
# your sessions, and unlike `clean` there is no plausible reading of "e2e-local" that
# includes it.
#
# The fix is to snapshot the matching pids before a test starts anything, then reap
# only what appeared afterwards. Matching by *name* cannot distinguish a leftover
# from somebody's live session; a pid that did not exist a moment ago can only be
# ours. (The same mistake, in miniature, produced a false failure in this repo's own
# e2e Part 7 when it identified a server with `pgrep -x etrs | head -1`.)
#
# Deliberately NOT `pgrep -f`: a full-command-line match also matches the shell
# running the pattern, which is the self-match trap that has bitten this fleet three
# times. `-x` matches the process name exactly.

# Snapshot the current pids for a process name, space-delimited and space-padded so
# a `case` glob can test membership without partial matches (" 123 " vs " 1234 ").
#
#   ETRS_PRE=$(procs_snapshot etrs)
procs_snapshot() {
    printf ' %s ' "$(pgrep -x -u "$(id -u)" "$1" 2>/dev/null | tr '\n' ' ')"
}

# Terminate the processes of that name which appeared after the snapshot.
#
#   procs_reap etrs "$ETRS_PRE"
#
# SIGTERM first, escalating to SIGKILL only for stragglers. Since v0.7.8 etrs exits
# promptly on SIGTERM, so the escalation should not normally fire -- if it starts
# firing, that is a signal the teardown path has regressed, not a reason to skip
# straight to SIGKILL.
procs_reap() {
    _pr_name=$1
    _pr_pre=$2
    _pr_new=""

    for _pr_pid in $(pgrep -x -u "$(id -u)" "$_pr_name" 2>/dev/null || true); do
        case "$_pr_pre" in
            *" $_pr_pid "*) : ;;                     # pre-existing -- not ours, leave it
            *) _pr_new="$_pr_new $_pr_pid" ;;
        esac
    done

    [ -n "$_pr_new" ] || return 0

    # shellcheck disable=SC2086
    kill -TERM $_pr_new 2>/dev/null || true

    for _ in 1 2 3 4 5 6; do
        _pr_left=""
        for _pr_pid in $_pr_new; do
            kill -0 "$_pr_pid" 2>/dev/null && _pr_left="$_pr_left $_pr_pid"
        done
        [ -n "$_pr_left" ] || { echo "stopped $_pr_name:$_pr_new"; return 0; }
        sleep 0.5
    done

    echo "warning: $_pr_name still alive after SIGTERM:$_pr_left -- sending SIGKILL" >&2
    # shellcheck disable=SC2086
    kill -9 $_pr_left 2>/dev/null || true
    echo "stopped $_pr_name (SIGKILL):$_pr_left"
}
