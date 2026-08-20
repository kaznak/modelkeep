{
  description = "ModelKeep persistent Hugging Face model mirror";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = function: nixpkgs.lib.genAttrs systems (system:
        function (import nixpkgs { inherit system; }));
    in {
      packages = forAllSystems (pkgs:
        let
          python = pkgs.python3.withPackages (pythonPackages: [ pythonPackages."huggingface-hub" ]);
          hfFetcher = pkgs.runCommand "modelkeep-hf-fetcher" {} ''
            mkdir -p $out/bin
            cp ${./upstream/hf_fetch.py} $out/bin/hf_fetch.py
            chmod 0555 $out/bin/hf_fetch.py
          '';
        in {
        modelkeep = pkgs.rustPlatform.buildRustPackage {
          pname = "modelkeep";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta.mainProgram = "modelkeep";
        };

        modelkeep-image = pkgs.dockerTools.buildLayeredImage {
          name = "modelkeep";
          tag = "0.1.0";
          contents = [ self.packages.${pkgs.system}.modelkeep hfFetcher python pkgs.cacert ];
          config = {
            Entrypoint = [ "${self.packages.${pkgs.system}.modelkeep}/bin/modelkeep" "serve" ];
            User = "10001:10001";
            ExposedPorts."8090/tcp" = {};
            Env = [ "RUST_LOG=info" "MODELKEEP_HF_PYTHON=${python}/bin/python3" "MODELKEEP_HF_HELPER=${hfFetcher}/bin/hf_fetch.py" ];
          };
        };

        default = self.packages.${pkgs.system}.modelkeep;
      });

      checks = forAllSystems (pkgs: {
        modelkeep = self.packages.${pkgs.system}.modelkeep;
      });
    };
}