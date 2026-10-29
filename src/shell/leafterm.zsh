# LeafTerm-only directory suggestions. Sourced after the user's zsh startup files.
[[ -o interactive && -o zle ]] || return 0
(( ${+_leafterm_cd_loaded} )) && return 0
typeset -g _leafterm_cd_loaded=1

if (( ! ${+_comps} )); then
  autoload -Uz compinit
  compinit -D -i
fi
zmodload zsh/complist || return 0
zstyle ':completion:*:cd:*' menu select

_leafterm_cd_context() {
  [[ -z $RBUFFER && ${#LBUFFER} -le 4096 ]] || return 1
  # Never run automatic completion while a compound command or expansion is
  # being written. Native manual Tab completion remains available in those cases.
  case $LBUFFER in
    (*'$'*|*'`'*|*';'*|*'|'*|*'&'*|*'<'*|*'>'*|*'('*|*')'*|*$'\n'*) return 1 ;;
  esac
  setopt localoptions extendedglob
  [[ $LBUFFER == [[:blank:]]#cd[[:blank:]]* ]]
}

_leafterm_cd_list_complete() {
  [[ ${words[1]} == cd ]] && (( CURRENT == 2 )) || return 0
  _main_complete
  # Listing must not insert the common prefix or pick the only matching folder.
  compstate[insert]=''
  if (( compstate[nmatches] > 200 )); then
    compstate[list]=''
    zle -M 'Many folders — type more of the path, or press Tab to browse.'
  else
    compstate[list]='list force'
    compstate[list_max]=0
  fi
}
zle -C _leafterm-cd-list list-choices _leafterm_cd_list_complete

_leafterm_cd_suggest() {
  _leafterm_cd_context || return 0
  setopt localoptions nobeep
  zle _leafterm-cd-list
  return 0
}
_leafterm_cd_self_insert() {
  zle _leafterm-original-self-insert -- "$@"
  _leafterm_cd_suggest
}
_leafterm_cd_magic_space() {
  zle _leafterm-original-magic-space -- "$@"
  _leafterm_cd_suggest
}
_leafterm_cd_backward_delete() {
  zle _leafterm-original-backward-delete-char -- "$@"
  _leafterm_cd_suggest
}
_leafterm_cd_paste() {
  zle _leafterm-original-bracketed-paste -- "$@"
  _leafterm_cd_suggest
}
zle -A self-insert _leafterm-original-self-insert
zle -A magic-space _leafterm-original-magic-space
zle -A backward-delete-char _leafterm-original-backward-delete-char
zle -A bracketed-paste _leafterm-original-bracketed-paste
zle -N self-insert _leafterm_cd_self_insert
zle -N magic-space _leafterm_cd_magic_space
zle -N backward-delete-char _leafterm_cd_backward_delete
zle -N bracketed-paste _leafterm_cd_paste
