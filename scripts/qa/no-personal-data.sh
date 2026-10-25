#!/usr/bin/env bash
# Nobody's name, nobody's home directory, nobody's machine, in anything that ships.
#
# This is software other people install. A test fixture with somebody's first name in it, a
# sample systemd unit naming its author's account and their home directory in three
# `Environment=` lines, an install script whose example path is one laptop's — each of those was
# written because it was what the author had in front of them, and each reaches every machine
# the project is installed on. One is worse than untidy: a sample unit naming a user who does
# not exist on the machine it is copied to does not start, and says nothing useful about why.
#
# Two things are checked:
#   - an absolute path under `/home/<somebody>`, anywhere but docs/;
#   - the words of whoever runs this: their login name, their machine's name, and each line of
#     `.git/info/personal-words` — a file git never tracks, so the list of what must not appear
#     does not itself appear anywhere. One word per line, `#` for comments; a line starting with
#     `!` is a phrase whose lines may keep the word (the author's own name in a copyright line).
#     On CI the login and the machine are the runner's, so there only the paths are checked.
#
#   no-personal-data.sh          every tracked file
#   no-personal-data.sh FILE…    just these (the pre-commit hook passes what is being committed)
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

# Only what is tracked: a build directory, a local scratch file or a private note is nobody
# else's business and is not shipped.
if [ "$#" -gt 0 ]; then
    files=("$@")
else
    mapfile -d '' -t files < <(git ls-files -z)
fi
[ "${#files[@]}" -eq 0 ] && exit 0
fail=0

homes=$(printf '%s\0' "${files[@]}" \
    | grep -zZv -e '^docs/' -e '^\.claude/' \
    | xargs -0r grep -nIH -e '/home/[a-z_][a-z0-9_-]*/' -- 2>/dev/null \
    | grep -v '/home/\$' \
    | grep -v '/home/TUUSUARIO/' \
    | grep -v '/home/<')
if [ -n "$homes" ]; then
    echo "A home directory of somebody's own, in a file that ships:" >&2
    echo "$homes" >&2
    echo "Write it as /home/TUUSUARIO/… , derive it at runtime, or take it out." >&2
    echo >&2
    fail=1
fi

words=()
allowed=()
if [ -z "${CI:-}" ]; then
    for own in "$(id -un 2>/dev/null)" "$(hostname 2>/dev/null)"; do
        case "$own" in
            '' | root | runner | user | ubuntu | admin | localhost) ;;
            *) [ "${#own}" -ge 3 ] && words+=("$own") ;;
        esac
    done
    list="$(git rev-parse --git-common-dir 2>/dev/null)/info/personal-words"
    if [ -f "$list" ]; then
        while IFS= read -r line || [ -n "$line" ]; do
            line="${line%%#*}"
            line="$(printf '%s' "$line" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
            case "$line" in
                '') ;;
                '!'*) allowed+=("$(printf '%s' "${line#!}" | sed -e 's/^[[:space:]]*//')") ;;
                *) words+=("$line") ;;
            esac
        done <"$list"
    fi
fi
if [ "${#words[@]}" -gt 0 ]; then
    patterns=()
    for word in "${words[@]}"; do patterns+=(-e "$word"); done
    named=$(printf '%s\0' "${files[@]}" | xargs -0r grep -nIHiwF "${patterns[@]}" -- 2>/dev/null)
    for phrase in "${allowed[@]}"; do
        named=$(printf '%s\n' "$named" | grep -vF -- "$phrase")
    done
    if [ -n "$named" ]; then
        echo "Your own name, login or machine, in a file that ships:" >&2
        echo "$named" >&2
        echo "Use a made-up name (nora, eva, marta…), or take it out." >&2
        echo >&2
        fail=1
    fi
fi

[ "$fail" -eq 0 ] && echo "Nothing personal in anything tracked (${#words[@]} own word(s) looked for)."
exit "$fail"
