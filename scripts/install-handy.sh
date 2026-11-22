#!/bin/bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
script="$repo/scripts/install-handy.sh"
bundle_id=com.jkuang.handy
legacy_bundle_id=com.jkuang.handy.send
app_name='Handy.app'
applications=/Applications
installed="$applications/$app_name"
legacy_installed="$applications/Handy Send.app"
updates="$applications/.handy-updates"
app_support="$HOME/Library/Application Support"
app_data="$app_support/$bundle_id"
legacy_data="$app_support/$legacy_bundle_id"

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

bundle_identifier() {
    /usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$1/Contents/Info.plist"
}

is_fork_id() {
    # The installer also accepts backups from before the ID migration.
    [[ "$1" == "$bundle_id" || "$1" == "$legacy_bundle_id" ]]
}

verify_bundle() {
    local identifier
    identifier=$(bundle_identifier "$1") && is_fork_id "$identifier" ||
        fail "Expected a signed bundle from this Handy fork at $1"
    /usr/bin/codesign --verify --deep --strict "$1"
}

already_installed() {
    [[ ! -L "$installed" ]] &&
        /usr/bin/cmp -s "$1/Contents/MacOS/handy" "$installed/Contents/MacOS/handy" &&
        /usr/bin/codesign --verify --deep --strict "$installed" 2>/dev/null
}

quit_handy() {
    /usr/bin/osascript -l JavaScript - "$bundle_id" "$legacy_bundle_id" <<'JXA'
ObjC.import('AppKit');

function run(argv) {
    const runningApps = () => {
        const running = [];
        for (const identifier of argv) {
            const apps = $.NSRunningApplication.runningApplicationsWithBundleIdentifier(identifier);
            for (let index = 0; index < apps.count; index++) running.push(apps.objectAtIndex(index));
        }
        return running;
    };
    for (const app of runningApps()) {
        app.terminate;
    }
    const deadline = Date.now() + 10000;
    while (runningApps().length > 0) {
        if (Date.now() >= deadline) throw new Error('Handy is still running. Quit it from the menu bar and retry.');
        $.NSRunLoop.currentRunLoop.runUntilDate($.NSDate.dateWithTimeIntervalSinceNow(0.1));
    }
}
JXA

    # The engine worker runs the same executable and can briefly outlive the app.
    local attempt status
    for ((attempt = 0; attempt < 40; attempt++)); do
        if /usr/bin/pgrep -f '^(/System/Volumes/Data)?/Applications/Handy( Send)?[.]app/Contents/MacOS/handy([[:space:]]|$)' >/dev/null; then
            sleep .25
        else
            status=$?
            [[ $status == 1 ]] || fail 'Could not verify that the Handy processes exited.'
            return
        fi
    done
    fail 'A Handy process is still running. Try again after it quits.'
}

rename_path() {
    /usr/bin/osascript -l JavaScript - "$1" "$2" "$3" <<'JXA'
ObjC.import('Foundation');
ObjC.bindFunction('renamex_np', ['int', ['char *', 'char *', 'unsigned int']]);

function run(argv) {
    const RENAME_SWAP = 2;
    const RENAME_EXCL = 4;
    const mode = argv[2] === 'swap' ? RENAME_SWAP : RENAME_EXCL;
    if ($.renamex_np(argv[0], argv[1], mode) !== 0) {
        throw new Error('Could not move ' + argv[0] + ' to ' + argv[1]);
    }
}
JXA
}

migrate_app_data() {
    [[ -d "$legacy_data" ]] || return 0
    if [[ -e "$app_data" || -L "$app_data" ]]; then
        [[ -d "$app_data" ]] || fail "Expected an app data directory at $app_data"
        printf 'Using existing Handy data at %s\n' "$app_data"
        return
    fi

    migration_stage=$(/usr/bin/mktemp -d "$app_support/.handy-migration.XXXXXX")
    printf 'Copying Handy data to %s…\n' "$app_data"
    /usr/bin/ditto "$legacy_data" "$migration_stage/data"
    rename_path "$migration_stage/data" "$app_data" move
    published_snapshot=true
}

cleanup_install() {
    local status=$?
    if [[ -n "$migration_stage" ]]; then
        if [[ "$published_snapshot" == true && "$status" != 0 ]] &&
            [[ $(bundle_identifier "$installed" 2>/dev/null || true) != "$bundle_id" ]]; then
            if rename_path "$app_data" "$migration_stage/data" move; then
                printf 'Copied data saved at %s\n' "$migration_stage/data" >&2
            else
                printf 'Copied data remains at %s\n' "$app_data" >&2
            fi
        else
            /bin/rm -rf -- "$migration_stage"
        fi
    fi
    if [[ "$preserve_stage" == false ]]; then
        /bin/rm -rf -- "$stage"
    fi
}

install_bundle() {
    local source=$1
    local previous="$installed"
    local previous_id=''
    [[ ! -L "$installed" ]] || fail "Expected an app bundle at $installed, found a symlink."
    if [[ ! -e "$installed" && -d "$legacy_installed" && ! -L "$legacy_installed" ]]; then
        previous="$legacy_installed"
    fi
    if [[ -e "$previous" ]]; then
        previous_id=$(bundle_identifier "$previous") && is_fork_id "$previous_id" ||
            fail "$previous belongs to a different build of Handy. Move it aside before installing this fork."
    fi
    if already_installed "$source"; then
        printf 'This Handy build is already installed.\n'
        return
    fi

    stage=$(/usr/bin/mktemp -d "$updates/update.XXXXXX")
    migration_stage=''
    published_snapshot=false
    preserve_stage=false
    trap cleanup_install EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    local candidate="$stage/$app_name"
    /usr/bin/ditto "$source" "$candidate"
    verify_bundle "$candidate"

    local previous_requirement=''
    if [[ -e "$previous" ]]; then
        previous_requirement=$(/usr/bin/codesign -d -r- "$previous" 2>&1 |
            /usr/bin/sed -n 's/^#\{0,1\} *designated => //p') || previous_requirement=''
    fi

    printf 'Waiting for Handy to quit…\n'
    quit_handy
    if [[ $(bundle_identifier "$candidate") == "$bundle_id" && "$previous_id" != "$bundle_id" ]]; then
        migrate_app_data
    fi
    # From this point the staging path may contain the previous install.
    preserve_stage=true
    if [[ -e "$installed" ]]; then
        rename_path "$installed" "$candidate" swap
    else
        rename_path "$candidate" "$installed" move
        if [[ -e "$previous" && "$previous" != "$installed" ]]; then
            rename_path "$previous" "$candidate" move
        fi
    fi
    if [[ -e "$candidate" ]]; then
        printf 'Previous app saved at %s\n' "$candidate"
    else
        /bin/rmdir "$stage"
    fi
    if [[ "$published_snapshot" == true ]]; then
        printf 'Original data saved at %s\n' "$legacy_data"
    fi

    if [[ -n "$previous_requirement" ]] &&
        ! /usr/bin/codesign --verify -R "=$previous_requirement" "$installed" 2>/dev/null; then
        printf 'The signing identity changed. macOS may ask you to grant access again.\n'
    fi
    /usr/bin/open "$installed"
}

if [[ $(/usr/bin/uname -s) != Darwin ]]; then
    fail 'This installer runs on macOS.'
fi

case ${1:-} in
    --help|-h)
        printf 'Usage: %s [BUNDLE.app | --build [CHECKOUT]]\n' "$0"
        printf 'With no arguments, builds and installs this checkout.\n'
        exit
        ;;
    --install-locked)
        install_bundle "$2"
        exit
        ;;
    ''|--build)
        [[ $# -le 2 ]] || fail 'Usage: install-handy.sh --build [CHECKOUT]'
        source=$(cd -- "${2:-$repo}" && pwd -P)
        [[ -f "$source/src-tauri/tauri.conf.json" ]] || fail "No Handy checkout at $source"
        if [[ -d "$legacy_installed" && ! -e "$installed" ]] &&
            ! /usr/bin/grep -q 'submit_to: Option<i32>' "$source/src-tauri/src/actions.rs"; then
            fail 'The earlier fork has Return-to-send. Pass a checkout that includes it to --build.'
        fi
        missing_tools=''
        for tool in cargo bun cmake; do
            command -v "$tool" >/dev/null || missing_tools+=" $tool"
        done
        [[ -z "$missing_tools" ]] || fail "Missing build tools:$missing_tools. See BUILD.md."
        target="$source/src-tauri/target"
        (
            cd -- "$source"
            bun install --frozen-lockfile
            CARGO_TARGET_DIR="$target" CMAKE_POLICY_VERSION_MINIMUM="${CMAKE_POLICY_VERSION_MINIMUM:-3.5}" \
                bun run tauri build --ci --bundles app \
                --config "{\"productName\":\"Handy\",\"identifier\":\"$bundle_id\",\"bundle\":{\"createUpdaterArtifacts\":false}}"
        )
        bundle="$target/release/bundle/macos/$app_name"
        ;;
    *)
        [[ $# == 1 ]] || fail 'Usage: install-handy.sh BUNDLE.app'
        bundle=$1
        [[ $bundle == /* ]] || bundle="$PWD/$bundle"
        ;;
esac

verify_bundle "$bundle"
if already_installed "$bundle"; then
    printf 'This Handy build is already installed.\n'
    exit
fi
/bin/mkdir -p "$updates"
exec /usr/bin/lockf -k -t 0 "$updates/install.lock" /bin/bash "$script" --install-locked "$bundle"
