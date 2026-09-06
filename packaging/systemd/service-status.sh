#!/usr/bin/env bash
# Emits systemctl status for a unit as JSON, shaped for a Discord embed like:
#   Unit / Status / Loaded / Active (+since) / Main PID / Tasks / Memory / CPU / CGroup / Processes
# plus a "targets" array with a TCP reachability check per [[redirect]] in
# the config file (UDP redirects are listed but not probed).
#
# Usage: service-status.sh <unit-name> [config-file]
#   unit-name    default: redir-rust.service
#   config-file  default: /etc/redir-rust/config.toml
set -euo pipefail

unit="${1:-redir-rust.service}"
config_file="${2:-/etc/redir-rust/config.toml}"

status_text="$(systemctl status --no-pager -l "$unit" 2>&1 || true)"

esc() { printf '%s' "$1" | sed -E 's/\\/\\\\/g; s/"/\\"/g'; }

get_field() {
    # First line starting with "  <label>:" -> everything after the colon, trimmed.
    printf '%s\n' "$status_text" | grep -m1 -E "^\s*${1}:" | sed -E "s/^\s*${1}:\s*//"
}

loaded="$(get_field 'Loaded')"
active_full="$(get_field 'Active')"
main_pid="$(get_field 'Main PID')"
tasks="$(get_field 'Tasks')"
memory="$(get_field 'Memory')"
cpu="$(get_field 'CPU')"
cgroup="$(printf '%s\n' "$status_text" | grep -m1 -E '^\s*CGroup:' | sed -E 's/^\s*CGroup:\s*//')"

# "Active" line looks like: active (running) since Wed 2026-07-01 13:13:00 UTC; 31min ago
active_summary="$(printf '%s' "$active_full" | sed -E 's/\s+since\s.*$//')"
active_state="$(printf '%s' "$active_summary" | cut -d' ' -f1)"
since="$(printf '%s' "$active_full" | sed -nE 's/.*\bsince (.*)$/\1/p')"

# Main PID line looks like: 2996715 (redir)
main_pid_num="$(printf '%s' "$main_pid" | sed -E 's/^([0-9]+).*/\1/')"

# Process tree: lines under CGroup that start with a tree-drawing prefix.
processes_json="[]"
if [[ -n "$cgroup" ]]; then
    mapfile -t proc_lines < <(printf '%s\n' "$status_text" \
        | awk '/^\s*CGroup:/{found=1; next} found && /[0-9]+ /{print}' \
        | sed -E 's/^[^0-9]*//')
    if ((${#proc_lines[@]} > 0)); then
        processes_json="["
        for i in "${!proc_lines[@]}"; do
            pid="$(printf '%s' "${proc_lines[$i]}" | sed -E 's/^([0-9]+).*/\1/')"
            command="$(printf '%s' "${proc_lines[$i]}" | sed -E 's/^[0-9]+\s+//')"
            processes_json+="{\"pid\": ${pid}, \"command\": \"$(esc "$command")\"}"
            ((i < ${#proc_lines[@]} - 1)) && processes_json+=","
        done
        processes_json+="]"
    fi
fi

# Per-redirect backend reachability. Parses [[redirect]] blocks out of the
# TOML config (only the top-level name/listen/target/protocol keys, which is
# all that's ambiguity-free with a plain line scan since the plugin
# sub-tables use disjoint key names).
check_tcp() {
    local host="$1" port="$2"
    timeout 2 bash -c "exec 3<>/dev/tcp/${host}/${port}" 2>/dev/null
}

targets_json="[]"
if [[ -f "$config_file" ]]; then
    mapfile -t redirect_lines < <(awk '
        function extract(line) {
            match(line, /"[^"]*"/)
            if (RSTART == 0) return ""
            return substr(line, RSTART + 1, RLENGTH - 2)
        }
        function flush() {
            if (listen != "") print name "\t" listen "\t" target "\t" protocol
            name = ""; listen = ""; target = ""; protocol = "tcp"
        }
        /^[ \t]*\[\[redirect\]\]/ { flush(); next }
        /^[ \t]*name[ \t]*=/      { name = extract($0) }
        /^[ \t]*listen[ \t]*=/    { listen = extract($0) }
        /^[ \t]*target[ \t]*=/    { target = extract($0) }
        /^[ \t]*protocol[ \t]*=/  { protocol = extract($0) }
        END { flush() }
    ' "$config_file")

    if ((${#redirect_lines[@]} > 0)); then
        targets_json="["
        for i in "${!redirect_lines[@]}"; do
            IFS=$'\t' read -r name listen target protocol <<<"${redirect_lines[$i]}"
            host="${target%:*}"
            port="${target##*:}"

            reachable="null"
            if [[ "$protocol" == "tcp" ]]; then
                if check_tcp "$host" "$port"; then reachable="true"; else reachable="false"; fi
            fi

            display_name="${name:-${listen} -> ${target}}"
            targets_json+="{\"name\": \"$(esc "$display_name")\", \"listen\": \"$(esc "$listen")\", \"target\": \"$(esc "$target")\", \"protocol\": \"$(esc "$protocol")\", \"reachable\": ${reachable}}"
            ((i < ${#redirect_lines[@]} - 1)) && targets_json+=","
        done
        targets_json+="]"
    fi
fi

# Recent connection-failure lines from the journal, e.g.:
#   Jul 01 05:12:52 host redir[2995858]: Failed connecting to target 100.85.222.77: Connection refused
# Works for both the Rust binary's tracing output and the legacy C `redir`
# tool's stderr, since both end up in the unit's journal either way.
errors_json="[]"
mapfile -t error_lines < <(journalctl -u "$unit" -n 200 --no-pager -o short-iso 2>/dev/null \
    | grep -iE 'failed|refused|error|timed? ?out|unreachable' \
    | tail -n 10)
if ((${#error_lines[@]} > 0)); then
    errors_json="["
    for i in "${!error_lines[@]}"; do
        errors_json+="\"$(esc "${error_lines[$i]}")\""
        ((i < ${#error_lines[@]} - 1)) && errors_json+=","
    done
    errors_json+="]"
fi

# Live "who's connected now" snapshot, written by the running redir-rust
# process itself (see src/connections.rs) to /run/redir-rust/connections.json
# whenever a connection opens/closes. Already valid JSON; passed through as-is.
connections_json="[]"
if [[ -s /run/redir-rust/connections.json ]]; then
    connections_json="$(cat /run/redir-rust/connections.json)"
fi

cat <<JSON
{
  "unit": "$(esc "$unit")",
  "status": "$(esc "$active_state")",
  "loaded": "$(esc "$loaded")",
  "active": "$(esc "$active_summary")",
  "since": "$(esc "$since")",
  "main_pid": "$(esc "$main_pid")",
  "main_pid_num": "$(esc "$main_pid_num")",
  "tasks": "$(esc "$tasks")",
  "memory": "$(esc "$memory")",
  "cpu": "$(esc "$cpu")",
  "cgroup": "$(esc "$cgroup")",
  "processes": $processes_json,
  "targets": $targets_json,
  "recent_errors": $errors_json,
  "active_connections": $connections_json
}
JSON
