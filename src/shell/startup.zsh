# Written as the private session ZDOTDIR/.zshenv by LeafTerm. Restore the
# user's ZDOTDIR before any of their startup files run; zsh then loads the
# remaining login/interactive files itself, in its normal order.
[[ -n ${LEAFTERM_SHELL_DIR-} ]] || return 0

builtin typeset -g _leafterm_completion_script="${LEAFTERM_SHELL_DIR}/leafterm.zsh"
builtin typeset -g _leafterm_startup_file="${LEAFTERM_SHELL_DIR}/.zshenv"
if [[ ${LEAFTERM_ZDOTDIR_WAS_SET-0} == 1 ]]; then
    builtin export ZDOTDIR="${LEAFTERM_ORIGINAL_ZDOTDIR-}"
else
    builtin unset ZDOTDIR
fi
builtin unset LEAFTERM_SHELL_DIR LEAFTERM_ZDOTDIR_WAS_SET LEAFTERM_ORIGINAL_ZDOTDIR

builtin typeset -g _leafterm_user_zshenv="${ZDOTDIR-$HOME}/.zshenv"
if [[ -r $_leafterm_user_zshenv && $_leafterm_user_zshenv != $_leafterm_startup_file ]]; then
    builtin source "$_leafterm_user_zshenv"
fi
builtin unset _leafterm_user_zshenv _leafterm_startup_file

if [[ -o interactive && -o rcs ]]; then
    # Install after .zshrc/.zlogin and their completion plugins have finished.
    # A configuration that replaces this hook array can opt out naturally.
    function _leafterm_deferred_completion_init() {
        builtin emulate -L zsh
        precmd_functions=("${(@)precmd_functions:#_leafterm_deferred_completion_init}")
        builtin local script=$_leafterm_completion_script
        builtin unset _leafterm_completion_script
        builtin unfunction _leafterm_deferred_completion_init
        [[ -r $script ]] && builtin source "$script"
        return 0
    }
    builtin typeset -ga precmd_functions
    precmd_functions=("${(@)precmd_functions:#_leafterm_deferred_completion_init}")
    precmd_functions+=(_leafterm_deferred_completion_init)
else
    builtin unset _leafterm_completion_script
fi
