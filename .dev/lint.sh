#!/usr/bin/env bash
# Rust lint set. Run from anywhere; checks the project this script sits in.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

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
# passes, and so does a shell comment after a `;;` that heads the next case arm.
# A Rust comment inside a method chain or a literal passes too, since rustfmt
# removes blank lines there: the next code line starts with `.`, or the code line
# before it ends with `,`, `(` or `[` and the next code line has no `=>`, since a
# match arm keeps its blank line. A Lua comment inside a method chain passes when
# the next code line starts with `:` or `.`, since stylua removes blank lines there.
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

# Latest stable edition. Bump when a new edition ships.
EDITION=2024

# lint_files <glob>...: print the project's files matching the globs, one per
# line. Inside a git work tree that is what git tracks or would track, so a
# nested checkout is never walked; elsewhere it is a ripgrep walk.
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
                case "$file" in */) continue ;; esac
                if [ -f "$file" ]; then
                    printf '%s\n' "$file"
                fi
            done
    else
        rg --files "${globs[@]}" -g '!target' .
    fi
}

# Tools

if cargo +nightly fmt --version >/dev/null 2>&1; then
    check "cargo fmt" cargo +nightly fmt --check
else
    missing "nightly rustfmt" "rustup toolchain install nightly --component rustfmt" "cargo fmt"
fi

check "cargo clippy" cargo clippy --all-targets --all-features -- -D warnings

if have ast-grep "brew install ast-grep" "ast-grep scan"; then
    sg_files=()
    while IFS= read -r file; do
        sg_files+=("$file")
    done < <(lint_files '*.rs')
    if [ "${#sg_files[@]}" -gt 0 ]; then
        check "ast-grep scan" ast-grep scan --config sgconfig.yml "${sg_files[@]}"
    fi
fi

if have cargo-sort "cargo install cargo-sort --locked" "cargo sort"; then
    check "cargo sort" cargo sort --check --workspace
fi

if have cargo-deny "cargo install cargo-deny --locked" "cargo deny"; then
    check "cargo deny" cargo deny check bans
fi

# Manifests

if have yq "brew install yq" manifests; then
    # field <file> <yq path>: print a value, or nothing when the key is absent.
    field() {
        yq -r -p toml -o json "$2 // \"\"" "$1"
    }

    published() {
        local value
        if [ "$(yq -p toml -o yaml '.package.publish | kind' "$1")" = "map" ]; then
            value=$(yq -p toml -o json -I0 '.workspace.package.publish' Cargo.toml)
        else
            value=$(yq -p toml -o json -I0 '.package.publish' "$1")
        fi
        case "$value" in
            false | '[]') return 1 ;;
        esac
    }

    # resolve <file> <key>: a [package] value, following `key.workspace = true`.
    resolve() {
        local kind
        kind=$(yq -p toml -o yaml ".package[\"$2\"] | kind" "$1")
        if [ "$kind" = "map" ]; then
            field Cargo.toml ".workspace.package[\"$2\"]"
        else
            field "$1" ".package[\"$2\"]"
        fi
    }

    manifests=$(lint_files 'Cargo.toml' | sort)

    printf '> manifests\n'
    for manifest in $manifests; do
        # Path dependencies need an exact pin.
        unpinned=$(yq -p toml -o json '
            [.dependencies, .["dev-dependencies"], .["build-dependencies"],
             .workspace.dependencies] | .[] | select(. != null) | to_entries | .[]
            | select(.value | tag == "!!map") | select(.value.path != null)
            | select((.value.version // "") | test("^=") | not) | .key' "$manifest" | tr -d '"')
        for dep in $unpinned; do
            fail manifests "$manifest: path dependency '$dep' needs an exact version, such as version = \"=1.2.3\""
        done

        banned=$(yq -p toml -o json '
            [.dependencies, .["dev-dependencies"], .["build-dependencies"],
             .workspace.dependencies, ((.target // {}) | .[] | .dependencies)]
            | .[] | select(. != null) | keys | .[]
            | select(test("^(once_cell|lazy_static|async-trait)$"))' "$manifest" | tr -d '"')
        for dep in $banned; do
            case "$dep" in
            async-trait) fail manifests "$manifest: depends on async-trait, write async trait methods as native async fn" ;;
            *) fail manifests "$manifest: depends on $dep, use std::sync::LazyLock" ;;
            esac
        done

        # Workspace-only manifests carry no [package].
        if [ "$(yq -p toml -o yaml '.package | kind' "$manifest")" != "map" ]; then
            edition=$(field "$manifest" '.workspace.package.edition')
            if [ -n "$edition" ] && [ "$edition" != "$EDITION" ]; then
                fail manifests "$manifest: edition is $edition, want $EDITION"
            fi
            continue
        fi

        edition=$(resolve "$manifest" edition)
        if [ "$edition" != "$EDITION" ]; then
            fail manifests "$manifest: edition is '${edition:-unset}', want $EDITION"
        fi

        rust_version=$(resolve "$manifest" rust-version)
        if ! published "$manifest"; then
            if [ -f rust-toolchain.toml ] && [ -n "$rust_version" ]; then
                fail manifests "$manifest: declares rust-version $rust_version, but the crate sets publish = false, so remove it and let rust-toolchain.toml pin the compiler"
            fi
        elif [ -z "$rust_version" ]; then
            fail manifests "$manifest: declares no rust-version, but the crate is published, since it does not set publish = false"
        fi
    done
fi

# Source patterns

if have rg "brew install ripgrep" "source patterns"; then
    canonical=(Copy Clone Debug Default PartialEq Eq PartialOrd Ord Hash)

    rs_files=()
    while IFS= read -r file; do
        rs_files+=("$file")
    done < <(lint_files '*.rs')

    printf '> derive order\n'
    while IFS= read -r hit; do
        [ -n "$hit" ] || continue
        file=${hit%%::*}
        rest=${hit#*::}
        location="$file:${rest%%::*}"
        line=${rest#*::}
        list=${line#*derive(}
        list=${list%%)*}
        last=-1
        in_order=1
        IFS=',' read -ra traits <<<"$list"
        for trait in "${traits[@]}"; do
            trait=${trait//[[:space:]]/}
            trait=${trait##*::}
            for i in "${!canonical[@]}"; do
                if [ "${canonical[$i]}" = "$trait" ]; then
                    if [ "$i" -lt "$last" ]; then
                        in_order=0
                    fi
                    last=$i
                fi
            done
        done
        if [ "$in_order" -eq 0 ]; then
            fail "derive order" "$location: derive list out of order, want Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash:$line"
        fi
    done < <(if [ "${#rs_files[@]}" -gt 0 ]; then rg --no-heading -n --with-filename --field-match-separator '::' 'derive\(' "${rs_files[@]}"; fi)

    printf '> module docs\n'
    while IFS= read -r file; do
        case "$(basename "$file")" in lib.rs | main.rs) continue ;; esac
        run=0
        start=0
        prev=0
        while IFS=: read -r number _; do
            if [ "$run" -gt 0 ] && [ "$number" -eq $((prev + 1)) ]; then
                run=$((run + 1))
            else
                if [ "$run" -gt 0 ] && [ "$run" -lt 4 ]; then
                    fail "module docs" "$file:$start: //! block is $run lines, so make it a 4+ line essay with a # section for an API module, or replace it with one /// line at the pub mod declaration"
                fi
                run=1
                start=$number
            fi
            prev=$number
        done < <(rg --no-heading -n '^\s*//!' "$file")
        if [ "$run" -gt 0 ] && [ "$run" -lt 4 ]; then
            fail "module docs" "$file:$start: //! block is $run lines, so make it a 4+ line essay with a # section for an API module, or replace it with one /// line at the pub mod declaration"
        fi
    done < <(lint_files '*.rs')

    printf '> cfg imports\n'
    for file in ${rs_files[@]+"${rs_files[@]}"}; do
        number=0
        gated=""
        while IFS= read -r line || [ -n "$line" ]; do
            number=$((number + 1))
            case "$line" in
                '#[cfg('*) [ -n "$gated" ] || gated=$number ;;
                '#['*) ;;
                'use '*)
                    if [ -n "$gated" ]; then
                        fail "cfg imports" "$file:$gated: a #[cfg]-gated use sits at the top level of the file, move it into the gated mod or function it serves"
                    fi
                    gated=""
                    ;;
                *) gated="" ;;
            esac
        done <"$file"
    done

    printf '> comment spacing\n'
    while IFS= read -r file; do
        case "$file" in
            *.rs) comment_spacing "$file" '//' ;;
            *.lua) comment_spacing "$file" '--' ;;
            *.sh) comment_spacing "$file" '#' ;;
        esac
    done < <(lint_files '*.rs' '*.lua' '*.sh')
fi

# Unsafe

if have rg "brew install ripgrep" unsafe && have yq "brew install yq" unsafe; then
    printf '> unsafe\n'
    allowed=" $(yq -p toml -o json '.workspace.metadata.style["allow-unsafe"] // [] | .[]
        | sub("^\./", "") | sub("/$", "")' Cargo.toml | tr -d '"' | tr '\n' ' ') "
    rs_files=()
    while IFS= read -r file; do
        rs_files+=("$file")
    done < <(lint_files '*.rs')
    while IFS= read -r hit; do
        [ -n "$hit" ] || continue
        file=${hit%%::*}
        file=${file#./}
        rest=${hit#*::}
        crate=$(dirname "$file")
        while [ "$crate" != "." ] && [ ! -f "$crate/Cargo.toml" ]; do
            crate=$(dirname "$crate")
        done
        case "$allowed" in *" $crate "*) continue ;; esac
        fail unsafe "$file:${rest%%::*}: allows unsafe_code, but [workspace.metadata.style] allow-unsafe in Cargo.toml does not list the crate at '$crate', and only the owner adds one"
    done < <(if [ "${#rs_files[@]}" -gt 0 ]; then rg --no-heading -n --with-filename --field-match-separator '::' '^\s*#!?\[.*\b(allow|expect)\s*\([^)]*\bunsafe_code\b' "${rs_files[@]}" || true; fi)
fi

# --- project-specific checks below ---

check "js syntax" node --check static/app.js
