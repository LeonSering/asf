# asf shell integration for zsh and bash.
#
# Source this file from ~/.zshrc or ~/.bashrc:
#     source /path/to/asf/shell/asf.sh
#
# When you open a session, asf writes the session folder and the command to a
# temp file. This function then changes into that folder in your current
# shell and runs the command, so you stay in the session folder after Claude
# Code exits.
asf() {
  local cmd_file="${TMPDIR:-/tmp}/asf-cmd-$$"
  rm -f "$cmd_file"
  ASF_PARENT_PID=$$ command asf "$@"
  if [ -f "$cmd_file" ]; then
    local target_dir open_cmd
    target_dir="$(sed -n '1p' "$cmd_file")"
    open_cmd="$(sed -n '2p' "$cmd_file")"
    rm -f "$cmd_file"
    if [ -n "$target_dir" ]; then
      cd "$target_dir" || return
    fi
    if [ -n "$open_cmd" ]; then
      eval "$open_cmd"
    fi
  fi
}
