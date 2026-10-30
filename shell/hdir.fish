# hdir shell integration for fish: `h` runs hdir and cd's to the directory the
# active panel was showing when you quit. Quit without cd (Shift+F10 / Ctrl+X)
# leaves you where you were.
#
# Install: add to ~/.config/fish/config.fish
#   source /path/to/hdir/shell/hdir.fish
function h
    set -l tmp (mktemp -t hdir.XXXXXX); or return
    command hdir --cwd-file $tmp $argv
    set -l rc $status
    set -l dir (cat -- $tmp | string collect)
    rm -f -- $tmp
    if test -n "$dir"; and test -d "$dir"; and test "$dir" != "$PWD"
        cd -- $dir
    end
    return $rc
end
