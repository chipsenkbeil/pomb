#!/usr/bin/env bash
# Rust lint set. Run from anywhere; checks the project this script sits in.
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source-path=SCRIPTDIR/.. source=lint-lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lint-lib.sh"

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Latest stable edition. Bump when a new edition ships.
EDITION=2024

# rs_pattern <check> <regex> <message>: fail on each line of the Rust files that
# matches the regex.
rs_pattern() {
    local hit file rest
    [ "${#rs_files[@]}" -gt 0 ] || return 0
    printf '> %s\n' "$1"
    while IFS= read -r hit; do
        [ -n "$hit" ] || continue
        file=${hit%%::*}
        rest=${hit#*::}
        fail "$1" "$file:${rest%%::*}: $3"
    done < <(rg --no-heading -n --with-filename --field-match-separator '::' "$2" "${rs_files[@]}" || true)
}

rs_files=()
while IFS= read -r file; do
    rs_files+=("$file")
done < <(lint_files '*.rs')

# Tools

if cargo +nightly fmt --version >/dev/null 2>&1; then
    check "cargo fmt" cargo +nightly fmt --check
else
    missing "nightly rustfmt" "rustup toolchain install nightly --component rustfmt" "cargo fmt"
fi

check "cargo clippy" cargo clippy --all-targets --all-features -- -D warnings

if [ "${#rs_files[@]}" -gt 0 ] && grep -qF debug_assertions "${rs_files[@]}"; then
    check "cargo clippy release" cargo clippy --release --all-targets --all-features -- -D warnings
fi

if have ast-grep "brew install ast-grep" "ast-grep scan"; then
    if [ "${#rs_files[@]}" -gt 0 ]; then
        check "ast-grep scan" ast-grep scan --config sgconfig.yml "${rs_files[@]}"
    fi
fi

if have cargo-sort "cargo install cargo-sort --locked" "cargo sort"; then
    check "cargo sort" cargo sort --check --workspace
fi

if have cargo-deny "cargo install cargo-deny --locked" "cargo deny"; then
    check "cargo deny" cargo deny check -W unmaintained
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

        thiserror=$(yq -p toml -o json '
            [.dependencies, ((.target // {}) | .[] | .dependencies)]
            | .[] | select(. != null) | keys | .[]
            | select(. == "thiserror")' "$manifest" | tr -d '"')
        if [ -n "$thiserror" ] && [ -f "$(dirname "$manifest")/src/lib.rs" ] && published "$manifest"; then
            fail thiserror "$manifest: a library crate depends on thiserror, write the error type by hand with Display and Error impls"
        fi

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

    printf '> comment periods\n'
    for file in ${rs_files[@]+"${rs_files[@]}"}; do
        while IFS= read -r number; do
            fail "comment periods" "$file:$number: a // one-liner ends in a period, drop it"
        done < <(comment_period_awk list "$file")
    done

    printf '> comment runs\n'
    for file in ${rs_files[@]+"${rs_files[@]}"}; do
        comment_runs "$file" '//'
    done

    rs_pattern getters '^\s*pub(\([^)]*\))?\s+(const\s+|async\s+|unsafe\s+)*fn\s+get_' "pub fn get_ names an accessor with a verb, name it after the noun"
    rs_pattern "doc headings" '^\s*//[/!]\s*##+\s' "a doc comment heading below h1, use a single # section"

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
check "passkey js syntax" node --check static/passkey.js
check "pages js syntax" node --check static/pages.js

# script_hits <message> <rg args...>: print each hit and fail on a hit or on an rg error (exit 2).
script_hits() {
    local message=$1 rc=0
    shift
    rg -n "$@" || rc=$?
    case "$rc" in
        1) return 0 ;;
        0) printf '%s\n' "$message" >&2 ;;
        *) printf 'rg failed with exit %s\n' "$rc" >&2 ;;
    esac
    return 1
}

# no_inline_scripts: fail when a template or a static script builds markup the page policy would block.
no_inline_scripts() {
    local found=0
    script_hits 'a template holds an inline <script>; move it into static/ (KF-609)' \
        -i -P '<script(?![^>]*\ssrc=)(?![^>]*\stype="application/json")' templates || found=1
    script_hits 'a template holds an inline event handler or javascript: URL; use data-call (KF-609)' \
        -i -P '\s(on[a-z]+|hx-on[a-z:-]*)=|javascript:|hx-vals=.js:' templates || found=1
    script_hits 'a static script builds an inline handler or <script> string (KF-609)' \
        -i -P '\son[a-z]+=\\?["'"'"']|<script' static/app.js static/pages.js static/passkey.js || found=1
    return "$found"
}

check "no inline scripts" no_inline_scripts

SHIPPED_TARGETS=(
    x86_64-unknown-linux-gnu
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-gnu
    aarch64-apple-darwin
    x86_64-apple-darwin
)

# no_c_build_tools: fail when a C build tool is in the tree for a shipped target.
no_c_build_tools() {
    local target tree found=0
    for target in "${SHIPPED_TARGETS[@]}"; do
        if ! tree=$(cargo tree --locked -e normal,build --target "$target" --prefix none); then
            printf 'cargo tree failed for %s\n' "$target" >&2
            found=1
        elif rg -q '^(cc|cmake|pkg-config|bindgen) v' <<<"$tree"; then
            printf 'a C build tool is in the dependency tree for %s\n' "$target" >&2
            found=1
        fi
    done
    return "$found"
}

check "no c build tools" no_c_build_tools

site_scripts=()
while IFS= read -r file; do
    site_scripts+=("$file")
done < <(lint_files '.dev/site/*.sh' '.dev/site/pomb-update')

if have shellcheck "brew install shellcheck" shellcheck; then
    check "shellcheck" shellcheck -x "${site_scripts[@]}"
fi

if have shfmt "brew install shfmt" shfmt; then
    check "shfmt" shfmt -d -i 2 -ci "${site_scripts[@]}"
fi

# private_details: fail on a public IPv4 address, a carrier-grade NAT address, a
# Tailscale host name or an absolute home path in any tracked text file. Documentation,
# loopback and private ranges pass, and so does any hit that a line of
# .dev/privacy-allow.txt matches (a regex against "file:line:match").
private_details() {
    local octet='(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])'
    local ip="(?<![0-9A-Za-z.§])${octet}(?:\\.${octet}){3}(?![0-9A-Za-z]|\\.[0-9])"
    local pattern="${ip}|\\.ts\\.net\\b|/(?:home|Users)/[a-z][a-z0-9_-]+/"
    local safe=':[0-9]+:(127\.|0\.0\.0\.0$|10\.|172\.(1[6-9]|2[0-9]|3[01])\.|192\.168\.|192\.0\.2\.|198\.51\.100\.|203\.0\.113\.)'
    local allow=.dev/privacy-allow.txt hits
    hits=$({ git ls-files -z 2>/dev/null || rg --files -0 --hidden -g '!.git' -g '!target'; } | { grep -zv -E '(^|/)(Cargo\.lock|target/.*)$|^scratchpad/' || true; } |
        { xargs -0 rg -P -o -n --no-heading --with-filename "$pattern" || true; } |
        { rg -v "$safe" || true; } |
        { rg -v -f <(rg -v '^\s*(#|$)' "$allow") || true; })
    [ -z "$hits" ] || {
        printf '%s\n' "$hits" >&2
        return 1
    }
}

check "private details" private_details
