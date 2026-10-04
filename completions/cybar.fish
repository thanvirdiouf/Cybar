# SPDX-License-Identifier: GPL-3.0-or-later
# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_cybar_global_optspecs
    string join \n config= timeout= h/help V/version
end

function __fish_cybar_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_cybar_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_cybar_using_subcommand
    set -l cmd (__fish_cybar_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c cybar -n "__fish_cybar_needs_command" -l config -r -F
complete -c cybar -n "__fish_cybar_needs_command" -l timeout -r
complete -c cybar -n "__fish_cybar_needs_command" -s h -l help -d 'Print help'
complete -c cybar -n "__fish_cybar_needs_command" -s V -l version -d 'Print version'
complete -c cybar -n "__fish_cybar_needs_command" -f -a "check" -d 'Validate the Lua configuration without opening an X window'
complete -c cybar -n "__fish_cybar_needs_command" -f -a "reload" -d 'Reload configuration; keep the current bars if validation fails'
complete -c cybar -n "__fish_cybar_needs_command" -f -a "quit" -d 'Stop the running instance and release its windows and tray selection'
complete -c cybar -n "__fish_cybar_needs_command" -f -a "toggle" -d 'Show or hide all bars, or one bar/monitor by name'
complete -c cybar -n "__fish_cybar_needs_command" -f -a "msg" -d 'Deliver a message to a named module without waiting for a reply'
complete -c cybar -n "__fish_cybar_needs_command" -f -a "request" -d 'Deliver a message and print the module handler\'s return value'
complete -c cybar -n "__fish_cybar_needs_command" -f -a "completions" -d 'Print shell completions'
complete -c cybar -n "__fish_cybar_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c cybar -n "__fish_cybar_using_subcommand check" -l config -r -F
complete -c cybar -n "__fish_cybar_using_subcommand check" -l timeout -r
complete -c cybar -n "__fish_cybar_using_subcommand check" -s h -l help -d 'Print help'
complete -c cybar -n "__fish_cybar_using_subcommand reload" -l config -r -F
complete -c cybar -n "__fish_cybar_using_subcommand reload" -l timeout -r
complete -c cybar -n "__fish_cybar_using_subcommand reload" -s h -l help -d 'Print help'
complete -c cybar -n "__fish_cybar_using_subcommand quit" -l config -r -F
complete -c cybar -n "__fish_cybar_using_subcommand quit" -l timeout -r
complete -c cybar -n "__fish_cybar_using_subcommand quit" -s h -l help -d 'Print help'
complete -c cybar -n "__fish_cybar_using_subcommand toggle" -l config -r -F
complete -c cybar -n "__fish_cybar_using_subcommand toggle" -l timeout -r
complete -c cybar -n "__fish_cybar_using_subcommand toggle" -s h -l help -d 'Print help'
complete -c cybar -n "__fish_cybar_using_subcommand msg" -l config -r -F
complete -c cybar -n "__fish_cybar_using_subcommand msg" -l timeout -r
complete -c cybar -n "__fish_cybar_using_subcommand msg" -s h -l help -d 'Print help'
complete -c cybar -n "__fish_cybar_using_subcommand request" -l config -r -F
complete -c cybar -n "__fish_cybar_using_subcommand request" -l timeout -r
complete -c cybar -n "__fish_cybar_using_subcommand request" -s h -l help -d 'Print help'
complete -c cybar -n "__fish_cybar_using_subcommand completions" -l config -r -F
complete -c cybar -n "__fish_cybar_using_subcommand completions" -l timeout -r
complete -c cybar -n "__fish_cybar_using_subcommand completions" -s h -l help -d 'Print help'
complete -c cybar -n "__fish_cybar_using_subcommand help; and not __fish_seen_subcommand_from check reload quit toggle msg request completions help" -f -a "check" -d 'Validate the Lua configuration without opening an X window'
complete -c cybar -n "__fish_cybar_using_subcommand help; and not __fish_seen_subcommand_from check reload quit toggle msg request completions help" -f -a "reload" -d 'Reload configuration; keep the current bars if validation fails'
complete -c cybar -n "__fish_cybar_using_subcommand help; and not __fish_seen_subcommand_from check reload quit toggle msg request completions help" -f -a "quit" -d 'Stop the running instance and release its windows and tray selection'
complete -c cybar -n "__fish_cybar_using_subcommand help; and not __fish_seen_subcommand_from check reload quit toggle msg request completions help" -f -a "toggle" -d 'Show or hide all bars, or one bar/monitor by name'
complete -c cybar -n "__fish_cybar_using_subcommand help; and not __fish_seen_subcommand_from check reload quit toggle msg request completions help" -f -a "msg" -d 'Deliver a message to a named module without waiting for a reply'
complete -c cybar -n "__fish_cybar_using_subcommand help; and not __fish_seen_subcommand_from check reload quit toggle msg request completions help" -f -a "request" -d 'Deliver a message and print the module handler\'s return value'
complete -c cybar -n "__fish_cybar_using_subcommand help; and not __fish_seen_subcommand_from check reload quit toggle msg request completions help" -f -a "completions" -d 'Print shell completions'
complete -c cybar -n "__fish_cybar_using_subcommand help; and not __fish_seen_subcommand_from check reload quit toggle msg request completions help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
