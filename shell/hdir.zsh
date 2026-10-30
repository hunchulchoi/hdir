# hdir shell integration for zsh: `h` runs hdir and cd's to the directory the
# active panel was showing when you quit. Quit without cd (Shift+F10 / Ctrl+X)
# leaves you where you were.
#
# Install: add to ~/.zshrc
#   source /path/to/hdir/shell/hdir.zsh
h() {
  local tmp dir rc
  tmp=$(mktemp -t hdir.XXXXXX) || return
  command hdir --cwd-file "$tmp" "$@"
  rc=$?
  dir=$(<"$tmp")
  rm -f -- "$tmp"
  if [[ -n $dir && -d $dir && $dir != "$PWD" ]]; then
    cd -- "$dir"
  fi
  return $rc
}
