# Nix Flake Usage

## Run

```bash
nix run github:felipemarques/rust-input

# With params
nix run github:felipemarques/rust-input -- --help

```

## Home-manager module

Add input:

```nix
inputs = {
    rustinput.url = "github:felipemarques/rust-input";
}
```

Enable rustinput:

``` nix
{
  inputs,
  ...
}: {
  # Add the Home Manager module
  imports = [inputs.rustinput.homeManagerModules.default];

  programs.rustinput = {
    enable = true;
    # systemd = false;
    # package = inputs.rustinput.packages.${pkgs.stdenv.hostPlatform.system}.default
    # Optional configuration in nix syntax, see config.toml for available options
    # settings = { };
    };
  };
}

```
