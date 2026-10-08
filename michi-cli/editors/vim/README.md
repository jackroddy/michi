# Vim and Neovim syntax for .michi

This directory is a Vim runtime layout with `syntax/michi.vim` and `ftdetect/michi.vim`.
To use it, add the directory to `runtimepath`, for example `set rtp+=/path/to/michi-cli/editors/vim` in your config.
Alternatively, symlink `syntax/michi.vim` into `~/.config/nvim/syntax/` and `ftdetect/michi.vim` into `~/.config/nvim/ftdetect/`.
Command text inside a step is highlighted with the stock `sh` syntax, so a colour scheme that styles shell also styles it here.
The command's end is found by regex, so a `;` inside nested `$( … )` or unusual quoting can end it early.
