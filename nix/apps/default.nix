{
  self',
  pkgs,
  neovimPackage,
  ...
}:

let
  neovim = pkgs.wrapNeovimUnstable neovimPackage {
    plugins = [
      {
        plugin = self'.packages.pterm;
        type = "lua";
        config = builtins.readFile ./nvim/pterm.lua;
      }
    ];
  };
in
{
  test-nvim = {
    type = "app";
    program = "${neovim}/bin/nvim";
    meta.description = "Neovim with pterm configured";
  };
}
