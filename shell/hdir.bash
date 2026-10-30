# hdir shell integration for bash: `h` runs hdir and cd's to the directory the
# active panel was showing when you quit. Quit without cd (Shift+F10 / Ctrl+X)
# leaves you where you were.
#
# Install: add to ~/.bashrc
#   source /path/to/hdir/shell/hdir.bash
h() {
  local tmp dir rc
  tmp=$(mktemp -t hdir.XXXXXX) || return
  command hdir --cwd-file "$tmp" "$@"
  rc=$?
  # The sentinel keeps trailing newlines, which $(...) would strip.
  dir=$(cat -- "$tmp"; printf x)
  dir=${dir%x}
  rm -f -- "$tmp"
  if [[ -n $dir && -d $dir && $dir != "$PWD" ]]; then
    cd -- "$dir" || return
  fi
  return $rc
}
