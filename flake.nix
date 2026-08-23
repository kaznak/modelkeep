{
  description = "ModelKeep persistent Hugging Face model mirror";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  # Fixed released writer used only by the cross-version archive compatibility check.
  inputs.modelkeepV021 = {
    url = "github:kaznak/modelkeep/1e7c82b5b0d0b5f89d463b18dbb6c4d2398367d4";
    flake = false;
  };

  outputs = { self, nixpkgs, modelkeepV021 }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = function: nixpkgs.lib.genAttrs systems (system:
        function (import nixpkgs { inherit system; }));
      pythonFor = pkgs:
        pkgs.python3.withPackages (pythonPackages: [
          pythonPackages."huggingface-hub"
        ]);
      rustToolsFor = pkgs: [
        pkgs.cargo
        pkgs.clippy
        pkgs.rustc
        pkgs.rustfmt
      ];
      cargoValidation = pkgs: name: command:
        pkgs.rustPlatform.buildRustPackage {
          pname = "modelkeep-${name}";
          version = "0.3.0";
          src = nixpkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = rustToolsFor pkgs;
          buildPhase = ''
            runHook preBuild
            ${command}
            runHook postBuild
          '';
          doCheck = false;
          installPhase = ''
            mkdir -p $out
            touch $out/passed
          '';
        };
    in {
      packages = forAllSystems (pkgs:
        let
          python = pythonFor pkgs;
          hfFetcher = pkgs.runCommand "modelkeep-hf-fetcher" {} ''
            mkdir -p $out/bin
            cp ${./upstream/hf_fetch.py} $out/bin/hf_fetch.py
            chmod 0555 $out/bin/hf_fetch.py
          '';
        in {
        modelkeep = pkgs.rustPlatform.buildRustPackage {
          pname = "modelkeep";
          version = "0.3.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta.mainProgram = "modelkeep";
        };

        modelkeep-image = pkgs.dockerTools.buildLayeredImage {
          name = "modelkeep";
          tag = "0.3.0";
          contents = [ self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep hfFetcher python pkgs.cacert pkgs.coreutils ];
          config = {
            Entrypoint = [ "${self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep}/bin/modelkeep" "serve" ];
            User = "10001:10001";
            ExposedPorts."8090/tcp" = {};
            Env = [ "RUST_LOG=info" "MODELKEEP_HF_PYTHON=${python}/bin/python3" "MODELKEEP_HF_HELPER=${hfFetcher}/bin/hf_fetch.py" ];
          };
        };

        qnap-client-acceptance = pkgs.writeShellApplication {
          name = "qnap-client-acceptance";
          runtimeInputs = [ python pkgs.cacert ];
          text = ''
            export SSL_CERT_FILE="${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
            exec python3 ${./tests/qnap_client_acceptance.py} "$@"
          '';
        };

        default = self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = rustToolsFor pkgs ++ [
            (pythonFor pkgs)
            pkgs.cacert
            pkgs.git
          ];
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        };
      });

      checks = forAllSystems (pkgs:
        let
          source = nixpkgs.lib.cleanSource ./.;
          python = pythonFor pkgs;
          oldModelkeep = pkgs.rustPlatform.buildRustPackage {
            pname = "modelkeep-upgrade-fixture";
            version = "0.2.1";
            src = modelkeepV021;
            cargoLock.lockFile = "${modelkeepV021}/Cargo.lock";
            doCheck = false;
          };
        in {
          format = pkgs.runCommand "modelkeep-format" {
            nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ];
          } ''
            cp -R ${source} source
            chmod -R u+w source
            cd source
            cargo fmt --check
            touch $out
          '';

          clippy = cargoValidation pkgs "clippy" ''
            cargo clippy --offline --all-targets --all-features -- -D warnings
          '';

          tests = cargoValidation pkgs "tests" ''
            cargo test --offline --all-features
          '';

          hf-client-environment = pkgs.runCommand "modelkeep-hf-client-environment" {
            nativeBuildInputs = [ python pkgs.cacert pkgs.git ];
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          } ''
            python3 -c "import huggingface_hub"
            hf --help >/dev/null
            git --version >/dev/null
            touch $out
          '';

          hf-fetcher-tests = pkgs.runCommand "modelkeep-hf-fetcher-tests" {
            nativeBuildInputs = [ python ];
          } ''
            cp ${./upstream/hf_fetch.py} hf_fetch.py
            cp ${./upstream/test_hf_fetch.py} test_hf_fetch.py
            python3 -m unittest -v test_hf_fetch.py
            touch $out
          '';

          hf-client-integration = pkgs.runCommand "modelkeep-hf-client-integration" {
            nativeBuildInputs = [
              python
              pkgs.cacert
              self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep
            ];
            SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          } ''
            export HOME="$TMPDIR"
            export HF_HOME="$TMPDIR/huggingface"
            python3 ${./tests/hf_client_integration.py} \
              ${self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep}/bin/modelkeep \
              ${./tests/fixtures/hf_fetch_fixture.py}
            touch $out
          '';

          archive-restore-drill = pkgs.runCommand "modelkeep-archive-restore-drill" {
            nativeBuildInputs = [ python self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep ];
          } ''
            python3 ${./tests/archive_restore_drill.py} \
              ${self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep}/bin/modelkeep
            touch $out
          '';

          archive-crash-upgrade = pkgs.runCommand "modelkeep-archive-crash-upgrade" {
            nativeBuildInputs = [
              pkgs.python3
              oldModelkeep
              self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep
            ];
          } ''
            python3 ${./tests/archive_crash_upgrade.py} \
              ${self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep}/bin/modelkeep \
              ${oldModelkeep}/bin/modelkeep \
              ${./tests/fixtures/hf_fetch_crash_fixture.py}
            touch $out
          '';

          archive-audit-cli = pkgs.runCommand "modelkeep-archive-audit-cli" {
            nativeBuildInputs = [ python self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep ];
          } ''
            python3 ${./tests/archive_audit_cli.py} \
              ${self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep}/bin/modelkeep
            touch $out
          '';

          qnap-client-acceptance-tests = pkgs.runCommand "modelkeep-qnap-client-acceptance-tests" {
            nativeBuildInputs = [ pkgs.python3 ];
          } ''
            cp ${./tests/qnap_client_acceptance.py} qnap_client_acceptance.py
            cp ${./tests/test_qnap_client_acceptance.py} test_qnap_client_acceptance.py
            python3 test_qnap_client_acceptance.py -v
            touch $out
          '';

          compose-network-boundary = pkgs.runCommand "modelkeep-compose-network-boundary" {
            nativeBuildInputs = [ pkgs.ripgrep pkgs.yq-go ];
          } ''
            port_mapping="$(yq -r '.services.modelkeep.ports[0]' ${./compose.yaml})"
            test "$port_mapping" = "127.0.0.1:8090:8090"
            test "$(yq -r '.services.modelkeep.ports[1]' ${./compose.yaml})" = "127.0.0.1:8091:8091"
            test "$(yq -r '.services.modelkeep.environment.MODELKEEP_ADMIN_ADDRESS' ${./compose.yaml})" = "0.0.0.0:8091"
            test "$(yq -r '.services.modelkeep.environment.MODELKEEP_TRUST_TAILSCALE_HEADERS' ${./compose.yaml})" = "true"
            image="$(yq -r '.services.modelkeep.image' ${./compose.yaml})"
            init_image="$(yq -r '.services."modelkeep-init".image' ${./compose.yaml})"
            test "$image" = "ghcr.io/kaznak/modelkeep:v0.3.0"
            test "$init_image" = "$image"
            test "$(yq -r '.services."modelkeep-init".container_name' ${./compose.yaml})" = "modelkeep-init"
            test "$(yq -r '.services.modelkeep.container_name' ${./compose.yaml})" = "modelkeep"
            test "$(yq -r '.services."modelkeep-init".user' ${./compose.yaml})" = "0:0"
            test "$(yq -r '.services."modelkeep-init".entrypoint | join(" ")' ${./compose.yaml})" = "/bin/modelkeep init-ownership /data"
            test "$(yq -r '.services."modelkeep-init".cap_add[0]' ${./compose.yaml})" = "CHOWN"
            test "$(yq -r '.services.modelkeep.depends_on."modelkeep-init".condition' ${./compose.yaml})" = "service_completed_successfully"
            test "$(yq -r '.services."modelkeep-init".volumes[0]' ${./compose.yaml})" = "/share/Services/modelkeep:/data"
            test "$(yq -r '.services.modelkeep.volumes[0]' ${./compose.yaml})" = "/share/Services/modelkeep:/data"
            if rg --fixed-strings '$' ${./compose.yaml}; then
              echo "compose.yaml must not require variable interpolation" >&2
              exit 1
            fi
            touch $out
          '';

          modelkeep = self.packages.${pkgs.stdenv.hostPlatform.system}.modelkeep;
        });
    };
}
