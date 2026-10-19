#!/usr/bin/env bash
# Shared helpers for the lint sets, sourced by each lint.sh.

status=0
failed=()

finish() {
    local rc=$?
    if [ "$rc" -ne 0 ]; then
        exit "$rc"
    fi
    if [ "$status" -ne 0 ]; then
        printf '\nFAILED: %s\n' "${failed[*]}" >&2
    fi
    exit "$status"
}
trap finish EXIT

# check <name> <command...>: run a command and record a failure without stopping.
check() {
    local name="$1"
    shift
    case "${VERBOSE:-}" in
        "" | 0 | no | false) printf '> %s\n' "$name" ;;
        *) printf '> %s: %s\n' "$name" "$*" ;;
    esac
    if ! "$@"; then
        status=1
        failed+=("$name")
    fi
}

# fail <name> <message>: record a failure found by an inline check.
fail() {
    printf '%s\n' "$2" >&2
    status=1
    failed+=("$1")
}

# comment_spacing <file> <marker>: fail on a full-line comment that directly
# follows a line of code at the same or a deeper indentation. A blank line,
# another comment line, an opening brace line or the top of the file before it
# passes, and so do the places where a formatter removes the blank line.
comment_spacing() {
    local file=$1 marker=$2 number=0 kind=none prev_indent="" prev_text="" in_block=0
    local line text indent is_comment pending="" literal=""
    while IFS= read -r line || [ -n "$line" ]; do
        number=$((number + 1))
        text=${line#"${line%%[![:space:]]*}"}
        indent=${line%"$text"}
        if [ "$in_block" -eq 1 ]; then
            case "$text" in *']]'*) in_block=0 ;; esac
            kind=comment
            continue
        fi
        if [ -z "$text" ]; then
            kind=blank
            continue
        fi
        is_comment=0
        case "$marker" in
            '//')
                case "$text" in
                    '///'* | '//!'*) kind=comment; continue ;;
                    '//'*) is_comment=1 ;;
                esac
                ;;
            '--')
                case "$text" in
                    '---'*) kind=comment; continue ;;
                    '--[['*)
                        is_comment=1
                        case "$text" in *']]'*) ;; *) in_block=1 ;; esac
                        ;;
                    '--'*) is_comment=1 ;;
                esac
                ;;
            '#')
                case "$text" in
                    '#!'*) kind=comment; continue ;;
                    '#'*) is_comment=1 ;;
                esac
                ;;
        esac
        if [ "$is_comment" -eq 1 ]; then
            if [ "$kind" = code ] && [ "${#indent}" -le "${#prev_indent}" ]; then
                case "$marker:$prev_text" in
                    '//:'*, | '//:'*'(' | '//:'*'[') literal=$number ;;
                    '#:;;' | '#:;&' | '#:;;&')
                        if [ "$indent" = "$prev_indent" ]; then
                            pending=$number
                        fi
                        ;;
                    *) pending=$number ;;
                esac
            fi
            kind=comment
        else
            if [ -n "$pending" ]; then
                case "$marker:$text" in
                    '//:.'* | '--::'* | '--:.'*) ;;
                    *) fail "comment spacing" "$file:$pending: full-line comment directly after code, add a blank line before it" ;;
                esac
                pending=""
            fi
            if [ -n "$literal" ]; then
                case "$text" in
                    *'=>'*) fail "comment spacing" "$file:$literal: full-line comment directly after code, add a blank line before it" ;;
                esac
                literal=""
            fi
            case "$text" in
                *'{') kind=open ;;
                *) kind=code ;;
            esac
            prev_indent=$indent
            prev_text=$text
        fi
    done <"$file"
    if [ -n "$pending" ]; then
        fail "comment spacing" "$file:$pending: full-line comment directly after code, add a blank line before it"
    fi
}

# comment_runs <file> <marker>: fail on three or more consecutive full-line
# comment lines, which the style reads as prose the code should carry. Doc
# comments (`///`, `//!`, `---`) and block comments are not counted, and a `//`
# run holding a numbered list item passes, since a list enumerates distinct cases.
comment_runs() {
    local file=$1 marker=$2 number=0 run=0 start=0 listed=0 line text is_comment
    while IFS= read -r line || [ -n "$line" ]; do
        number=$((number + 1))
        text=${line#"${line%%[![:space:]]*}"}
        case "$marker:$text" in
            '//:///'* | '//://!'* | '--:---'* | '--:--[['*) is_comment=0 ;;
            "$marker:$marker"*) is_comment=1 ;;
            *) is_comment=0 ;;
        esac
        if [ "$is_comment" -eq 1 ]; then
            if [ "$run" -eq 0 ]; then
                start=$number
                listed=0
            fi
            run=$((run + 1))
            case "$marker:$text" in
                '//:// '[0-9]*) listed=1 ;;
            esac
            continue
        fi
        if [ "$run" -ge 3 ] && [ "$listed" -eq 0 ]; then
            fail "comment runs" "$file:$start: $run consecutive comment lines, restructure the code or shrink the comment"
        fi
        run=0
    done <"$file"
    if [ "$run" -ge 3 ] && [ "$listed" -eq 0 ]; then
        fail "comment runs" "$file:$start: $run consecutive comment lines, restructure the code or shrink the comment"
    fi
}

# have <binary> <install command> [check name]: true when the binary exists,
# else one WARNING naming the check it skips, or a failure under CI, which never
# skips a check. The check name defaults to the binary.
have() {
    if command -v "$1" >/dev/null 2>&1; then
        return 0
    fi
    missing "$1" "$2" "${3:-$1}"
    return 1
}

missing() {
    if [ -n "${CI:-}" ]; then
        fail "$3" "$1 not found, so the $3 check cannot run, and CI never skips a check. Install with: $2"
        return
    fi
    printf 'WARNING: %s not found, skipping the %s check. Install with: %s\n' "$1" "$3" "$2" >&2
}

# lint_files <glob>...: print the project's files matching the globs, one per
# line, never under a target directory. Inside a git work tree that is what git
# tracks or would track, so a nested checkout is never walked; elsewhere it is a
# ripgrep walk. A project sets LINT_SKIP_HIDDEN to leave hidden paths out.
lint_files() {
    local glob file
    local -a specs=() globs=()
    for glob in "$@"; do
        specs+=(":(glob)**/$glob")
        globs+=(-g "$glob")
    done
    if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git ls-files --cached --others --exclude-standard -z -- "${specs[@]}" ':(exclude,glob)**/target/**' |
            while IFS= read -r -d '' file; do
                case "$file" in
                    */) continue ;;
                    .* | */.*) [ -z "${LINT_SKIP_HIDDEN:-}" ] || continue ;;
                esac
                if [ -f "$file" ]; then
                    printf '%s\n' "$file"
                fi
            done
    elif [ -n "${LINT_SKIP_HIDDEN:-}" ]; then
        rg --files --no-hidden "${globs[@]}" -g '!target' .
    else
        rg --files "${globs[@]}" -g '!target' .
    fi
}
