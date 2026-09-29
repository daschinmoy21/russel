//! `russel init` — scaffold a starter `Russelfile.toml` (and optional `flake.nix`).
//!
//! The generated manifest matches the current `russel-core` TOML schema so
//! `russel deploy` can consume it immediately. Flake templates follow the
//! language detection used by control-plane auto-generation (Rust / Go /
//! static) but are written as a committed starter, not the restricted-mode
//! "do not edit" marker.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use russel_core::{
    RuntimeKind,
    config::{Russelfile, validate_source_path},
};

const RUSSELFILE_NAME: &str = "Russelfile.toml";
const FLAKE_NAME: &str = "flake.nix";
const DEFAULT_PORT: u16 = 3000;
const DEFAULT_MEMORY: &str = "256mb";
const FALLBACK_NAME: &str = "app";

/// Create a starter `Russelfile.toml` (and optionally `flake.nix`).
#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    /// Directory to initialize (created if missing). Defaults to the current directory.
    #[arg(value_name = "DIR", default_value = ".")]
    pub path: PathBuf,

    /// Service name (default: inferred from Cargo.toml, go.mod, or the directory name).
    #[arg(long)]
    pub name: Option<String>,

    /// Guest listen port written to `service.port`.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// Memory limit written to `service.memory` (e.g. 256mb).
    #[arg(long, default_value = DEFAULT_MEMORY)]
    pub memory: String,

    /// Runtime kind written to `service.type`: `container` (default) or
    /// `microvm` (experimental; needs /dev/kvm and passt on the ctrl host).
    #[arg(long = "type", visible_alias = "runtime", value_name = "RUNTIME")]
    pub runtime: Option<RuntimeKind>,

    /// nixpkgs attribute for services without a flake
    /// (e.g. `--package navidrome`). Works with either `--type`.
    #[arg(long, value_name = "ATTR")]
    pub package: Option<String>,

    /// Binary name produced by the build (default: service name).
    #[arg(long)]
    pub bin: Option<String>,

    /// Also write a starter `flake.nix` for the detected project type.
    #[arg(long)]
    pub with_flake: bool,

    /// Overwrite existing `Russelfile.toml` / `flake.nix`.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectKind {
    Rust,
    Go,
    Static,
}

/// How `buildGoModule` should treat dependencies in a generated flake.
///
/// `vendorHash = null` tells nixpkgs to skip the module FOD and use only a
/// committed `vendor/` directory (or a stdlib-only module). External modules
/// without `vendor/` need a real hash; `lib.fakeHash` makes the first
/// `nix build` print it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoVendorMode {
    Null,
    FakeHash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Created,
    Overwritten,
    LeftInPlace,
}

#[derive(Debug, Clone)]
struct ProjectHints {
    kind: ProjectKind,
    inferred_name: Option<String>,
    go_vendor: GoVendorMode,
}

/// Run `russel init`.
pub fn run(args: InitArgs) -> Result<()> {
    let root = resolve_target_dir(&args.path)?;
    fs::create_dir_all(&root)
        .with_context(|| format!("failed to create directory {}", root.display()))?;
    let root = fs::canonicalize(&root)
        .with_context(|| format!("failed to resolve directory {}", root.display()))?;

    let hints = detect_project(&root);
    let mut name = resolve_name(args.name.as_deref(), hints.inferred_name.as_deref(), &root)?;
    if let Some(package) = args.package.as_deref() {
        russel_core::volumes::validate_package_attr(package.trim())
            .map_err(|e| anyhow!("invalid --package {package:?}: {e}"))?;
    }
    let package = args
        .package
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if package.is_some() && args.with_flake {
        bail!(
            "--package cannot be combined with --with-flake (committed flake wins over package; omit --with-flake to deploy the nixpkgs attr)"
        );
    }
    let mut bin = match (args.bin.as_deref(), package) {
        (Some(explicit), _) => resolve_bin(Some(explicit), &name)?,
        (None, Some(pkg)) => resolve_bin(Some(pkg.rsplit('.').next().unwrap_or(pkg)), &name)?,
        (None, None) => resolve_bin(None, &name)?,
    };
    validate_port(args.port)?;
    validate_memory(&args.memory)?;
    validate_source_path(".")?;

    let russelfile_path = root.join(RUSSELFILE_NAME);
    let flake_path = root.join(FLAKE_NAME);

    let russelfile_existed = path_exists(&russelfile_path)?;
    let flake_existed = path_exists(&flake_path)?;

    if !args.force && russelfile_existed && !args.with_flake {
        bail!(
            "{} already exists (use --force to overwrite)",
            russelfile_path.display()
        );
    }
    if !args.force && args.with_flake && russelfile_existed && flake_existed {
        bail!(
            "{} and {} already exist (use --force to overwrite)",
            russelfile_path.display(),
            flake_path.display()
        );
    }

    let keep_russelfile = russelfile_existed && !args.force;
    let mut port = args.port;
    let mut memory = args.memory.clone();
    // Container is the default; an explicit --type still applies. The
    // package auto-flake builds for either runtime.
    let mut runtime = args.runtime.unwrap_or_default();
    let mut keep_package: Option<String> = package.map(str::to_string);
    if keep_russelfile {
        let existing = Russelfile::load(&russelfile_path)
            .with_context(|| format!("failed to read {}", russelfile_path.display()))?;
        bin = existing.service.bin_name().to_string();
        name = existing.service.name;
        port = existing.service.port;
        memory = format!("{}mb", existing.service.memory.as_mebibytes());
        runtime = existing.service.runtime;
        keep_package = existing.service.package.clone();
    }

    let russelfile_backup = if !keep_russelfile && russelfile_existed {
        Some(fs::read(&russelfile_path).with_context(|| {
            format!(
                "failed to read {} before overwrite",
                russelfile_path.display()
            )
        })?)
    } else {
        None
    };
    let russelfile_created = !keep_russelfile && !russelfile_existed;

    let rf_outcome = if keep_russelfile {
        WriteOutcome::LeftInPlace
    } else {
        let manifest =
            render_russelfile(&name, port, &memory, runtime, &bin, keep_package.as_deref());
        // Fail closed: never write a manifest the current parser would reject.
        Russelfile::load_from_str(&manifest)
            .context("internal error: generated Russelfile.toml failed to parse")?;
        write_text_file(&russelfile_path, &manifest, args.force, russelfile_existed)?
    };

    let flake_outcome = match write_flake(
        &args,
        hints.kind,
        &name,
        &bin,
        hints.go_vendor,
        &flake_path,
        flake_existed,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            if let Err(restore_err) = restore_russelfile(
                &russelfile_path,
                russelfile_backup.as_deref(),
                russelfile_created,
            ) {
                return Err(err.context(restore_err));
            }
            return Err(err);
        }
    };

    print_summary(
        &root,
        &name,
        &bin,
        port,
        &memory,
        runtime,
        hints.kind,
        rf_outcome,
        flake_outcome,
        args.with_flake,
        hints.kind == ProjectKind::Rust && !root.join("Cargo.lock").exists(),
        hints.kind == ProjectKind::Go && hints.go_vendor == GoVendorMode::FakeHash,
    );

    Ok(())
}

fn resolve_target_dir(path: &Path) -> Result<PathBuf> {
    let expanded = expand_home(path)?;
    if let Ok(meta) = fs::symlink_metadata(&expanded) {
        if meta.file_type().is_symlink() {
            let target = fs::canonicalize(&expanded)
                .with_context(|| format!("failed to resolve directory {}", expanded.display()))?;
            if !target.is_dir() {
                bail!("{} is not a directory", expanded.display());
            }
            return Ok(target);
        }
        if !meta.file_type().is_dir() {
            bail!("{} exists and is not a directory", expanded.display());
        }
        return fs::canonicalize(&expanded)
            .with_context(|| format!("failed to resolve directory {}", expanded.display()));
    }
    // Path does not exist yet — create_dir_all in the caller, then canonicalize.
    // Return the expanded path as-is (may still be relative).
    Ok(expanded)
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let raw = path.as_os_str();
    let Some(s) = raw.to_str() else {
        return Ok(path.to_path_buf());
    };
    if let Some(rest) = s.strip_prefix('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            bail!("cannot expand '~' in path: $HOME is not set");
        }
        return Ok(PathBuf::from(format!("{home}{rest}")));
    }
    Ok(path.to_path_buf())
}

fn detect_project(root: &Path) -> ProjectHints {
    if root.join("Cargo.toml").is_file() {
        let inferred_name = fs::read_to_string(root.join("Cargo.toml"))
            .ok()
            .and_then(|c| cargo_package_name(&c));
        return ProjectHints {
            kind: ProjectKind::Rust,
            inferred_name,
            go_vendor: GoVendorMode::Null,
        };
    }
    if root.join("go.mod").is_file() {
        let contents = fs::read_to_string(root.join("go.mod")).unwrap_or_default();
        let inferred_name = go_module_basename(&contents);
        return ProjectHints {
            kind: ProjectKind::Go,
            inferred_name,
            go_vendor: detect_go_vendor_mode(root, &contents),
        };
    }
    ProjectHints {
        kind: ProjectKind::Static,
        inferred_name: None,
        go_vendor: GoVendorMode::Null,
    }
}

fn detect_go_vendor_mode(root: &Path, go_mod: &str) -> GoVendorMode {
    // A committed vendor tree is what vendorHash = null is for.
    if root.join("vendor").is_dir() {
        return GoVendorMode::Null;
    }
    if go_mod_has_external_require(go_mod) || go_sum_has_modules(root) {
        return GoVendorMode::FakeHash;
    }
    GoVendorMode::Null
}

fn strip_go_line_comment(line: &str) -> &str {
    let trimmed = line.trim();
    if trimmed.starts_with("//") {
        return "";
    }
    match trimmed.find("//") {
        Some(i) => trimmed[..i].trim(),
        None => trimmed,
    }
}

/// True when `go.mod` lists at least one module `require` (direct or indirect).
fn go_mod_has_external_require(contents: &str) -> bool {
    let mut in_require = false;
    for line in contents.lines() {
        let t = strip_go_line_comment(line);
        if t.is_empty() {
            continue;
        }
        if in_require {
            if t == ")" || t.starts_with(')') {
                in_require = false;
                continue;
            }
            if t.split_whitespace().next().is_some() {
                return true;
            }
            continue;
        }
        let Some(rest) = t.strip_prefix("require") else {
            continue;
        };
        // Word boundary: do not match identifiers like `required`.
        if !rest.is_empty() && !rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
            continue;
        }
        let rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix('(') {
            in_require = true;
            let after = after.trim();
            if !after.is_empty() && after != ")" {
                return true;
            }
            continue;
        }
        if !rest.is_empty() {
            return true;
        }
    }
    false
}

fn go_sum_has_modules(root: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(root.join("go.sum")) else {
        return false;
    };
    contents.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with("//")
    })
}

fn cargo_package_name(contents: &str) -> Option<String> {
    let mut in_package = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("name") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        return unquote_toml_string(rest);
    }
    None
}

fn unquote_toml_string(raw: &str) -> Option<String> {
    let s = raw.split('#').next().unwrap_or(raw).trim();
    if let Some(inner) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        return Some(inner.to_string());
    }
    if let Some(inner) = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        return Some(inner.to_string());
    }
    None
}

fn go_module_basename(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        let rest = trimmed.strip_prefix("module ")?.trim();
        if rest.is_empty() {
            return None;
        }
        let last = rest.rsplit('/').next().filter(|s| !s.is_empty())?;
        return Some(last.to_string());
    }
    None
}

fn resolve_name(explicit: Option<&str>, inferred: Option<&str>, root: &Path) -> Result<String> {
    if let Some(name) = explicit {
        return validate_service_name(name.trim()).map(str::to_string);
    }
    if let Some(name) = inferred {
        let sanitized = sanitize_ident(name);
        if !sanitized.is_empty() {
            return Ok(avoid_reserved(sanitized));
        }
    }
    let dir_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(FALLBACK_NAME);
    let sanitized = sanitize_ident(dir_name);
    if sanitized.is_empty() {
        return Ok(FALLBACK_NAME.to_string());
    }
    Ok(avoid_reserved(sanitized))
}

fn resolve_bin(explicit: Option<&str>, name: &str) -> Result<String> {
    let bin = explicit
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    russel_core::ids::validate_bin_name(bin)?;
    Ok(bin.to_string())
}

/// `--name` follows the same rule `Russelfile::load` applies to
/// `service.name`: the service id rule in `russel_core::ids`.
fn validate_service_name(name: &str) -> Result<&str> {
    russel_core::ids::validate_service_id(name)
        .map_err(|e| anyhow!("invalid service name {name:?}: {e}"))?;
    Ok(name)
}

fn validate_port(port: u16) -> Result<()> {
    if port == 0 {
        bail!("--port must not be 0");
    }
    Ok(())
}

fn validate_memory(raw: &str) -> Result<()> {
    if raw.is_empty() || raw.contains('"') || raw.contains('\n') || raw.contains('\r') {
        bail!("invalid --memory {raw:?} (expected e.g. 256mb)");
    }
    let stub =
        format!("[service]\nname = \"app\"\nsource = \".\"\nport = 3000\nmemory = \"{raw}\"\n");
    Russelfile::load_from_str(&stub)
        .map(|_| ())
        .map_err(|e| anyhow!("invalid --memory {raw:?}: {e}"))
}

fn sanitize_ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_sep = false;
    for c in raw.chars() {
        let mapped = if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            Some(c)
        } else if c == '.' || c == ' ' {
            Some('-')
        } else {
            None
        };
        let Some(ch) = mapped else {
            continue;
        };
        let sep = ch == '-' || ch == '_';
        if sep && (prev_sep || out.is_empty()) {
            prev_sep = sep;
            continue;
        }
        out.push(ch);
        prev_sep = sep;
    }
    out.trim_end_matches(['-', '_']).to_string()
}

fn avoid_reserved(name: String) -> String {
    if russel_core::reserved::is_reserved_service_dir(&name) {
        format!("{name}-svc")
    } else {
        name
    }
}

fn render_russelfile(
    name: &str,
    port: u16,
    memory: &str,
    runtime: RuntimeKind,
    bin: &str,
    package: Option<&str>,
) -> String {
    let package_line = package
        .map(|p| format!("\n# nixpkgs attr used when no flake.nix exists. Committed flake wins.\npackage = \"{p}\"\n"))
        .unwrap_or_default();
    let volumes_block = if package.is_some() {
        "\n# Managed data dir (/var/lib/russel/<id>/volumes/data). keep=true survives destroy\n# unless --delete-volumes. Absolute host binds are never deleted.\n# [[volumes]]\n# name = \"data\"\n# guest = \"/data\"\n# rw = true\n# keep = true\n"
    } else {
        ""
    };
    format!(
        "\
# Generated by `russel init`. Uncomment optional fields below as needed.
#
# Init flags (re-run with --force to overwrite this file):
#   russel init [DIR]
#   --name NAME              service.name   (default: Cargo.toml / go.mod / directory)
#   --port PORT              service.port   (default: 3000, must not be 0)
#   --memory SIZE            service.memory (default: 256mb; suffix mb or mib, min 16mb)
#   --type / --runtime KIND  service.type   (container | microvm, default: container)
#   --bin BIN                service.bin    (default: service.name, or package last part)
#   --package ATTR           nixpkgs attr to wrap when there is no flake.nix
#   --with-flake             also write flake.nix (Rust / Go / static)
#   --force                  overwrite existing Russelfile.toml / flake.nix
#
# Deploy / operate:
#   russel deploy . --config Russelfile.toml
#   (service id, env, ingress, and podman flags all come from this file)
#   printf '%s' \"$VAL\" | russel secrets set NAME
#   russel secrets list
#   russel status {name}
#   russel logs {name}
#   russel stop {name}
#   russel destroy {name}
#   russel destroy {name} --keep-volumes    # keep managed [[volumes]] dirs
#   russel destroy {name} --delete-volumes  # delete managed [[volumes]] dirs

[service]
# Unique service name and service id. Default binary name if `bin` is omitted.
# Same rule as a service id: A-Za-z0-9_- (max 128), not secrets/traefik/_pool.
name = \"{name}\"

# Source directory relative to the repo root. \".\" is this project.
# Must be relative; \"..\" and absolute paths are rejected.
source = \".\"

# Guest listen port. The process should bind this (Russel also sets PORT).
# Must not be 0. Set [ingress].port to pin a host-side backend port.
port = {port}

# Memory limit (microVM RAM / container --memory). Suffix mb or mib. Minimum 16mb.
# Recommended: 256mb for Go/Rust, 512mb–1024mb for heavier runtimes. No gb suffix.
memory = \"{memory}\"

# Runtime source of truth.
#   container  — rootless Podman --rootfs (default; typical VPS / no KVM)
#   microvm    — KVM / Cloud Hypervisor, experimental: needs /dev/kvm and
#                passt on the ctrl host (no root)
type = \"{runtime}\"
{package_line}
# Guest userspace. Orthogonal to type (isolation). Default busybox.
# linux is parsed but rejected until implemented (host-built NixOS userspace).
# guest = \"busybox\"

# Binary produced by the Nix build, executed as $out/bin/<bin>.
# Defaults to `name` when omitted. Allowed: A-Za-z0-9._+- (max 256).
bin = \"{bin}\"
{volumes_block}
# CPUs (1..=32, default 1): microVM vCPUs, container --cpus limit.
# cpus = 1

# Container-only: include bash, curl, and /usr/bin/env in the rootfs.
# Default false (hardened: ELF entrypoint or an absolute /nix/store interpreter).
# Required if the entrypoint is a `#!/usr/bin/env bash` script.
# debug = false

# Deploy-time environment.
# Keys: ^[A-Za-z_][A-Za-z0-9_]*$. Max 64 keys, 4096 bytes per value.
# Reserved (cannot set): PORT, VM_IP, HOST_IP, APP, IFS, PATH, LD_PRELOAD,
# LD_AUDIT, LD_LIBRARY_PATH, BASH_ENV, ENV, SHELL. Checked when the file loads.
# Values: plain text, or secret://NAME from `russel secrets set NAME`.
# [service.env]
# LOG_LEVEL = \"info\"
# FEATURE_X = \"1\"
# DATABASE_URL = \"secret://DATABASE_URL\"

# Optional HTTP ingress. Omit for Host(<service_id>.<RUSSEL_TRAEFIK_DOMAIN>)
# and an allocated backend port.
# host is the exact Traefik Host() value (apex or subdomain), not a suffix.
# port is the host-side backend, not service.port.
# [ingress]
# host = \"abc.com\"
# port = 4000
"
    )
}

fn render_flake(kind: ProjectKind, name: &str, bin: &str, go_vendor: GoVendorMode) -> String {
    // Committed starter: no auto-generation marker (restricted Nix mode
    // refuses files that start with that banner). Keep pname/bin interpolated
    // values restricted to the validated charset so Nix strings stay safe.
    match kind {
        ProjectKind::Rust => format!(
            "\
# Starter flake written by `russel init --with-flake`.
# Russel deploys packages.<system>.default. Edit freely.
{{
  description = \"Russel project: {name}\";

  inputs.nixpkgs.url = \"github:NixOS/nixpkgs/nixos-unstable\";

  outputs = {{ self, nixpkgs }}:
    let
      systems = [ \"x86_64-linux\" \"aarch64-linux\" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {{
      packages = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${{system}};
        in {{
          default = pkgs.rustPlatform.buildRustPackage {{
            pname = \"{bin}\";
            version = \"0.1.0\";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
          }};
        }});

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${{system}};
        in {{
          default = pkgs.mkShell {{
            packages = [ pkgs.rustc pkgs.cargo pkgs.rustfmt pkgs.rust-analyzer ];
          }};
        }});
    }};
}}
"
        ),
        ProjectKind::Go => {
            let vendor_attr = match go_vendor {
                GoVendorMode::Null => {
                    "\
            # null is correct for stdlib-only modules or a committed vendor/ directory.
            vendorHash = null;"
                }
                GoVendorMode::FakeHash => {
                    "\
            # External modules and no vendor/. null skips fetching and the sandbox build fails.
            # First `nix build` / deploy prints got: sha256-... — paste it here.
            # Or run `go mod vendor` and set vendorHash = null.
            vendorHash = pkgs.lib.fakeHash;"
                }
            };
            format!(
                "\
# Starter flake written by `russel init --with-flake`.
# Russel deploys packages.<system>.default. Edit freely.
{{
  description = \"Russel project: {name}\";

  inputs.nixpkgs.url = \"github:NixOS/nixpkgs/nixos-unstable\";

  outputs = {{ self, nixpkgs }}:
    let
      systems = [ \"x86_64-linux\" \"aarch64-linux\" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {{
      packages = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${{system}};
        in {{
          default = pkgs.buildGoModule {{
            pname = \"{bin}\";
            version = \"0.1.0\";
            src = ./.;
{vendor_attr}
          }};
        }});

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${{system}};
        in {{
          default = pkgs.mkShell {{
            packages = [ pkgs.go pkgs.gopls pkgs.gotools ];
          }};
        }});
    }};
}}
"
            )
        }
        ProjectKind::Static => format!(
            "\
# Starter flake written by `russel init --with-flake`.
# Russel deploys packages.<system>.default. Edit freely.
{{
  description = \"Russel project: {name}\";

  inputs.nixpkgs.url = \"github:NixOS/nixpkgs/nixos-unstable\";

  outputs = {{ self, nixpkgs }}:
    let
      systems = [ \"x86_64-linux\" \"aarch64-linux\" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {{
      packages = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${{system}};
        in {{
          default = pkgs.writeShellScriptBin \"{bin}\" ''
            cd ${{./.}}
            exec ${{pkgs.python3}}/bin/python3 -m http.server \"$PORT\"
          '';
        }});

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${{system}};
        in {{
          default = pkgs.mkShell {{
            packages = [ pkgs.python3 ];
          }};
        }});
    }};
}}
"
        ),
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn write_flake(
    args: &InitArgs,
    kind: ProjectKind,
    name: &str,
    bin: &str,
    go_vendor: GoVendorMode,
    flake_path: &Path,
    flake_existed: bool,
) -> Result<Option<WriteOutcome>> {
    if !args.with_flake {
        return Ok(None);
    }
    if !args.force && flake_existed {
        return Ok(Some(WriteOutcome::LeftInPlace));
    }
    let flake = render_flake(kind, name, bin, go_vendor);
    Ok(Some(write_text_file(
        flake_path,
        &flake,
        args.force,
        flake_existed,
    )?))
}

fn restore_russelfile(path: &Path, backup: Option<&[u8]>, created: bool) -> Result<()> {
    if let Some(bytes) = backup {
        fs::write(path, bytes).with_context(|| {
            format!(
                "failed to restore {} after flake write failed",
                path.display()
            )
        })?;
    } else if created {
        fs::remove_file(path).with_context(|| {
            format!(
                "failed to remove newly written {} after flake write failed",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn write_text_file(
    path: &Path,
    contents: &str,
    force: bool,
    existed: bool,
) -> Result<WriteOutcome> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("refusing to write through symlink {}", path.display());
            }
            if !meta.file_type().is_file() {
                bail!("{} exists and is not a regular file", path.display());
            }
            if !force {
                return Ok(WriteOutcome::LeftInPlace);
            }
            fs::write(path, contents)
                .with_context(|| format!("failed to overwrite {}", path.display()))?;
            Ok(WriteOutcome::Overwritten)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            let mut file = opts
                .open(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
            file.write_all(contents.as_bytes())
                .with_context(|| format!("failed to write {}", path.display()))?;
            Ok(if existed {
                WriteOutcome::Overwritten
            } else {
                WriteOutcome::Created
            })
        }
        Err(e) => Err(e).with_context(|| format!("failed to stat {}", path.display())),
    }
}

#[allow(clippy::too_many_arguments)]
fn print_summary(
    root: &Path,
    name: &str,
    bin: &str,
    port: u16,
    memory: &str,
    runtime: RuntimeKind,
    kind: ProjectKind,
    rf_outcome: WriteOutcome,
    flake_outcome: Option<WriteOutcome>,
    with_flake: bool,
    warn_missing_lock: bool,
    warn_go_vendor_hash: bool,
) {
    println!();
    println!("  \x1b[1;36mrussel init\x1b[0m");
    println!("  \x1b[2m{}\x1b[0m", root.display());
    println!();
    step("name", name);
    step("type", &runtime.to_string());
    step("port", &port.to_string());
    step("memory", memory);
    step("bin", bin);
    step(
        "stack",
        match kind {
            ProjectKind::Rust => "rust",
            ProjectKind::Go => "go",
            ProjectKind::Static => "static",
        },
    );
    println!();
    file_line(RUSSELFILE_NAME, rf_outcome);
    if let Some(outcome) = flake_outcome {
        file_line(FLAKE_NAME, outcome);
    }
    println!();
    println!("  \x1b[1mNext:\x1b[0m");
    println!("    russel deploy .");
    match runtime {
        RuntimeKind::Container => {
            println!("    \x1b[2m(container runtime — needs rootless Podman)\x1b[0m");
        }
        RuntimeKind::Microvm => {
            println!(
                "    \x1b[2m(microVM runtime is experimental — needs /dev/kvm and passt on the ctrl host)\x1b[0m"
            );
        }
    }
    if !with_flake {
        println!();
        println!(
            "    \x1b[2mNo flake.nix written. Deploy will auto-generate one, or re-run:\x1b[0m"
        );
        println!("    russel init --with-flake");
    }
    if warn_missing_lock && with_flake {
        println!();
        println!(
            "  \x1b[1;33mwarning:\x1b[0m no Cargo.lock found; `nix build` / deploy will need one"
        );
        println!("    cargo generate-lockfile");
    }
    if warn_go_vendor_hash && with_flake {
        println!();
        println!("  \x1b[1;33mwarning:\x1b[0m go.mod has external modules and no vendor/");
        println!("    flake.nix uses lib.fakeHash — first `nix build` / deploy prints vendorHash");
        println!("    paste that hash, or run `go mod vendor` and set vendorHash = null");
    }
    println!();
}

fn step(label: &str, value: &str) {
    println!("  \x1b[2m{label:>10}\x1b[0m  \x1b[1m{value}\x1b[0m");
}

fn file_line(name: &str, outcome: WriteOutcome) {
    let verb = match outcome {
        WriteOutcome::Created => "wrote",
        WriteOutcome::Overwritten => "overwrote",
        WriteOutcome::LeftInPlace => "kept",
    };
    println!("  \x1b[2m{verb:>10}\x1b[0m  {name}");
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;
    use russel_core::GuestKind;
    use russel_core::config::Russelfile;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(Debug, clap::Subcommand)]
    enum TestCommand {
        Init(InitArgs),
    }

    fn parse_init(args: &[&str]) -> InitArgs {
        let mut argv = vec!["russel", "init"];
        argv.extend(args);
        match TestCli::try_parse_from(argv).unwrap().command {
            TestCommand::Init(init) => init,
        }
    }

    #[test]
    fn cargo_package_name_reads_package_table() {
        let toml = r#"
[workspace]
members = ["crates/foo"]

[package]
name = "hello-rust"
version = "0.1.0"
"#;
        assert_eq!(cargo_package_name(toml).as_deref(), Some("hello-rust"));
    }

    #[test]
    fn cargo_package_name_ignores_workspace_only() {
        let toml = "[workspace]\nmembers = [\"a\"]\n";
        assert_eq!(cargo_package_name(toml), None);
    }

    #[test]
    fn go_module_basename_uses_last_path_component() {
        assert_eq!(
            go_module_basename("module github.com/acme/basic-http\n\ngo 1.22\n").as_deref(),
            Some("basic-http")
        );
        assert_eq!(
            go_module_basename("// comment\nmodule foo\n").as_deref(),
            Some("foo")
        );
        assert_eq!(go_module_basename("go 1.22\n"), None);
    }

    #[test]
    fn go_mod_detects_single_and_block_requires() {
        assert!(!go_mod_has_external_require(
            "module example.com/svc\n\ngo 1.22\n"
        ));
        assert!(!go_mod_has_external_require(
            "module example.com/svc\n// require github.com/foo/bar v1.0.0\n"
        ));
        assert!(!go_mod_has_external_require(
            "module example.com/svc\nrequired = true\n"
        ));
        assert!(go_mod_has_external_require(
            "module example.com/svc\nrequire github.com/foo/bar v1.2.3\n"
        ));
        assert!(go_mod_has_external_require(
            "module example.com/svc\nrequire (\n\tgithub.com/foo/bar v1.2.3\n)\n"
        ));
        assert!(go_mod_has_external_require(
            "module example.com/svc\nrequire (\n\tgolang.org/x/sys v0.1.0 // indirect\n)\n"
        ));
    }

    #[test]
    fn go_vendor_mode_prefers_vendor_dir_then_requires() {
        let dir = tempfile::tempdir().unwrap();
        let go_mod = "module example.com/svc\nrequire github.com/foo/bar v1.0.0\n";
        assert_eq!(
            detect_go_vendor_mode(dir.path(), go_mod),
            GoVendorMode::FakeHash
        );
        fs::create_dir(dir.path().join("vendor")).unwrap();
        assert_eq!(
            detect_go_vendor_mode(dir.path(), go_mod),
            GoVendorMode::Null
        );
    }

    #[test]
    fn go_vendor_mode_treats_go_sum_as_external_modules() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("go.sum"),
            "github.com/foo/bar v1.0.0 h1:abc=\n",
        )
        .unwrap();
        assert_eq!(
            detect_go_vendor_mode(dir.path(), "module example.com/svc\ngo 1.22\n"),
            GoVendorMode::FakeHash
        );
    }

    #[test]
    fn sanitize_ident_strips_noise() {
        assert_eq!(sanitize_ident("My App!"), "My-App");
        assert_eq!(sanitize_ident("...dots..."), "dots");
        assert_eq!(sanitize_ident("___"), "");
        assert_eq!(sanitize_ident("hello-rust"), "hello-rust");
    }

    #[test]
    fn avoid_reserved_suffixes_host_dirs() {
        assert_eq!(avoid_reserved("secrets".into()), "secrets-svc");
        assert_eq!(avoid_reserved("api".into()), "api");
    }

    #[test]
    fn generated_manifest_parses() {
        let body = render_russelfile(
            "my-app",
            3000,
            "256mb",
            RuntimeKind::Container,
            "my-app",
            None,
        );
        let cfg = Russelfile::load_from_str(&body).unwrap();
        assert_eq!(cfg.service.name, "my-app");
        assert_eq!(cfg.service.port, 3000);
        assert_eq!(cfg.service.bin_name(), "my-app");
        assert_eq!(cfg.service.runtime, RuntimeKind::Container);
        assert_eq!(cfg.service.guest, GuestKind::Busybox);
        assert_eq!(cfg.service.source, ".");
        assert_eq!(cfg.service.memory.as_mebibytes(), 256);
        assert!(!cfg.service.debug);
        assert_eq!(cfg.service.cpus, 1);
        assert!(cfg.service.env.is_empty());
    }

    #[test]
    fn generated_manifest_documents_schema_and_cli() {
        let body = render_russelfile(
            "my-app",
            8080,
            "512mb",
            RuntimeKind::Container,
            "custom-bin",
            None,
        );
        for needle in [
            "name = \"my-app\"",
            "source = \".\"",
            "port = 8080",
            "memory = \"512mb\"",
            "type = \"container\"",
            "# guest = \"busybox\"",
            "linux is parsed but rejected until implemented",
            "bin = \"custom-bin\"",
            "# cpus = 1",
            "# debug = false",
            "# [service.env]",
            "# LOG_LEVEL = \"info\"",
            "secret://DATABASE_URL",
            "# Optional HTTP ingress. Omit for Host(<service_id>.<RUSSEL_TRAEFIK_DOMAIN>)",
            "# and an allocated backend port.",
            "# host is the exact Traefik Host() value (apex or subdomain), not a suffix.",
            "# port is the host-side backend, not service.port.",
            "# [ingress]",
            "# host = \"abc.com\"",
            "# port = 4000",
            "--name NAME",
            "--port PORT",
            "--memory SIZE",
            "--type / --runtime KIND",
            "--bin BIN",
            "--with-flake",
            "--force",
            "russel deploy .",
            "--config Russelfile.toml",
            "russel secrets set NAME",
            "russel status my-app",
            "russel logs my-app",
            "russel stop my-app",
            "russel destroy my-app",
            "PORT, VM_IP, HOST_IP, APP",
            "LD_AUDIT",
            "A-Za-z0-9_- (max 128)",
            "512mb–1024mb",
        ] {
            assert!(
                body.contains(needle),
                "generated Russelfile missing {needle:?}\n{body}"
            );
        }
        // Only mb / mib parse; a gb hint would recommend an invalid value.
        assert!(
            !body.contains("1gb"),
            "template must not suggest gb:\n{body}"
        );
    }

    #[test]
    fn rust_flake_mentions_cargo_lock_and_dev_shell() {
        let flake = render_flake(ProjectKind::Rust, "api", "api", GoVendorMode::Null);
        assert!(flake.contains("buildRustPackage"));
        assert!(flake.contains("cargoLock.lockFile"));
        assert!(flake.contains("devShells"));
        assert!(flake.contains("pname = \"api\""));
        assert!(!flake.contains("Auto-generated by Russel"));
    }

    #[test]
    fn go_flake_uses_build_go_module() {
        let flake = render_flake(ProjectKind::Go, "api", "basic-http", GoVendorMode::Null);
        assert!(flake.contains("buildGoModule"));
        assert!(flake.contains("vendorHash = null"));
        assert!(flake.contains("pname = \"basic-http\""));
        assert!(!flake.contains("fakeHash"));
    }

    #[test]
    fn go_flake_uses_fake_hash_when_modules_are_unvendored() {
        let flake = render_flake(ProjectKind::Go, "api", "api", GoVendorMode::FakeHash);
        assert!(flake.contains("buildGoModule"));
        assert!(flake.contains("vendorHash = pkgs.lib.fakeHash"));
        assert!(!flake.contains("vendorHash = null;"));
    }

    #[test]
    fn static_flake_serves_on_port() {
        let flake = render_flake(ProjectKind::Static, "site", "site", GoVendorMode::Null);
        assert!(flake.contains("http.server"));
        assert!(flake.contains("$PORT"));
        assert!(flake.contains("${./.}"));
    }

    #[test]
    fn init_writes_russelfile_in_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap();

        let body = fs::read_to_string(dir.path().join(RUSSELFILE_NAME)).unwrap();
        let cfg = Russelfile::load_from_str(&body).unwrap();
        assert_eq!(cfg.service.name, "demo");
        assert_eq!(cfg.service.bin_name(), "demo");
        assert!(!dir.path().join(FLAKE_NAME).exists());
    }

    #[test]
    fn init_infers_name_from_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"hello-rust\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: None,
            port: 8080,
            memory: "128mb".into(),
            runtime: Some(RuntimeKind::Container),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.name, "hello-rust");
        assert_eq!(cfg.service.port, 8080);
        assert_eq!(cfg.service.runtime, RuntimeKind::Container);
        assert_eq!(cfg.service.memory.as_mebibytes(), 128);
    }

    #[test]
    fn init_infers_name_from_go_mod() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("go.mod"),
            "module github.com/acme/shortlink\n",
        )
        .unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: None,
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.name, "shortlink");
    }

    #[test]
    fn init_infers_name_from_directory() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("cool-app");
        fs::create_dir(&dir).unwrap();
        run(InitArgs {
            path: dir.clone(),
            name: None,
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.name, "cool-app");
    }

    #[test]
    fn init_refuses_existing_russelfile_without_force() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(RUSSELFILE_NAME), "already here").unwrap();
        let err = run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "unexpected err: {err}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(RUSSELFILE_NAME)).unwrap(),
            "already here"
        );
    }

    #[test]
    fn init_force_overwrites_russelfile() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(RUSSELFILE_NAME), "stale").unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("fresh".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: true,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.name, "fresh");
    }

    #[test]
    fn init_with_flake_writes_detected_template() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module example.com/svc\n").unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: None,
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: true,
            force: false,
        })
        .unwrap();
        let flake = fs::read_to_string(dir.path().join(FLAKE_NAME)).unwrap();
        assert!(flake.contains("buildGoModule"));
        assert!(flake.contains("pname = \"svc\""));
        assert!(
            flake.contains("vendorHash = null"),
            "stdlib-only go.mod should keep vendorHash = null, got:\n{flake}"
        );
    }

    #[test]
    fn init_with_flake_uses_fake_hash_for_unvendored_go_modules() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("go.mod"),
            "module example.com/svc\n\ngo 1.22\n\nrequire github.com/foo/bar v1.2.3\n",
        )
        .unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: None,
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: true,
            force: false,
        })
        .unwrap();
        let flake = fs::read_to_string(dir.path().join(FLAKE_NAME)).unwrap();
        assert!(
            flake.contains("vendorHash = pkgs.lib.fakeHash"),
            "unvendored Go modules should use fakeHash, got:\n{flake}"
        );
        assert!(!flake.contains("vendorHash = null;"));
    }

    #[test]
    fn init_with_flake_keeps_existing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(RUSSELFILE_NAME),
            "[service]\nname = \"kept\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\n",
        )
        .unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("ignored".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: true,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.name, "kept");
        let flake = fs::read_to_string(dir.path().join(FLAKE_NAME)).unwrap();
        assert!(
            flake.contains("pname = \"kept\"") || flake.contains("writeShellScriptBin \"kept\""),
            "flake should use the existing Russelfile name, got:\n{flake}"
        );
    }

    #[test]
    fn init_force_with_flake_restores_manifest_when_flake_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let russelfile = dir.path().join(RUSSELFILE_NAME);
        let original =
            "[service]\nname = \"kept\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\n";
        fs::write(&russelfile, original).unwrap();
        // Directory named flake.nix: write_text_file refuses non-regular files.
        fs::create_dir(dir.path().join(FLAKE_NAME)).unwrap();
        let err = run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("replaced".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: true,
            force: true,
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a regular file") || msg.contains("flake.nix"),
            "got: {msg}"
        );
        let after = fs::read_to_string(&russelfile).unwrap();
        assert_eq!(after, original, "existing Russelfile.toml must be restored");
    }

    #[test]
    fn init_creates_missing_directory() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("brand-new");
        run(InitArgs {
            path: dir.clone(),
            name: Some("brand-new".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap();
        assert!(dir.join(RUSSELFILE_NAME).is_file());
    }

    #[test]
    fn init_rejects_file_as_target() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        let err = run(InitArgs {
            path: file,
            name: Some("demo".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn init_refuses_symlink_russelfile() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.toml");
        fs::write(&target, "x").unwrap();
        let link = dir.path().join(RUSSELFILE_NAME);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: true,
        })
        .unwrap_err();
        assert!(err.to_string().contains("symlink"), "unexpected err: {err}");
    }

    #[test]
    fn init_rejects_port_zero() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 0,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("must not be 0"));
    }

    #[test]
    fn init_rejects_bad_memory() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 3000,
            memory: "512gb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("invalid --memory"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn init_rejects_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("has space".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("service name"));
    }

    #[test]
    fn clap_parses_init_flags() {
        let args = parse_init(&[
            "./svc",
            "--name",
            "svc",
            "--port",
            "8080",
            "--memory",
            "512mb",
            "--type",
            "container",
            "--with-flake",
            "--force",
        ]);
        assert_eq!(args.path, PathBuf::from("./svc"));
        assert_eq!(args.name.as_deref(), Some("svc"));
        assert_eq!(args.port, 8080);
        assert_eq!(args.memory, "512mb");
        assert_eq!(args.runtime, Some(RuntimeKind::Container));
        assert!(args.with_flake);
        assert!(args.force);
    }

    #[test]
    fn clap_accepts_runtime_alias() {
        let args = parse_init(&["--runtime", "container"]);
        assert_eq!(args.runtime, Some(RuntimeKind::Container));
    }

    #[test]
    fn explicit_bin_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("my-app".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: Some("custom-bin".into()),
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.bin_name(), "custom-bin");
    }

    #[test]
    fn runtime_defaults_to_container() {
        let dir = tempfile::tempdir().unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: None,
            bin: None,
            package: None,
            with_flake: false,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.runtime, RuntimeKind::Container);
    }

    #[test]
    fn package_implies_container_runtime() {
        let dir = tempfile::tempdir().unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("navidrome".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: None,
            bin: None,
            package: Some("navidrome".into()),
            with_flake: false,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.runtime, RuntimeKind::Container);
        assert_eq!(cfg.service.package.as_deref(), Some("navidrome"));
        assert_eq!(cfg.service.bin_name(), "navidrome");
    }

    #[test]
    fn package_works_with_explicit_microvm() {
        let dir = tempfile::tempdir().unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: Some(RuntimeKind::Microvm),
            bin: None,
            package: Some("navidrome".into()),
            with_flake: false,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.runtime, RuntimeKind::Microvm);
        assert_eq!(cfg.service.package.as_deref(), Some("navidrome"));
    }

    #[test]
    fn package_bin_defaults_to_last_attr_segment() {
        let dir = tempfile::tempdir().unwrap();
        run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: None,
            bin: None,
            package: Some("nodePackages.foo".into()),
            with_flake: false,
            force: false,
        })
        .unwrap();
        let cfg = Russelfile::load(&dir.path().join(RUSSELFILE_NAME)).unwrap();
        assert_eq!(cfg.service.package.as_deref(), Some("nodePackages.foo"));
        assert_eq!(cfg.service.bin_name(), "foo");
        assert_eq!(cfg.service.runtime, RuntimeKind::Container);
    }

    #[test]
    fn render_russelfile_with_package_loads_package_set() {
        let body = render_russelfile(
            "demo",
            3000,
            "256mb",
            RuntimeKind::Container,
            "foo",
            Some("nodePackages.foo"),
        );
        let cfg = Russelfile::load_from_str(&body).unwrap();
        assert_eq!(cfg.service.package.as_deref(), Some("nodePackages.foo"));
        assert_eq!(cfg.service.bin_name(), "foo");
        assert_eq!(cfg.service.runtime, RuntimeKind::Container);
        assert!(body.contains("package = \"nodePackages.foo\""));
        assert!(body.contains("[[volumes]]"));
    }

    #[test]
    fn package_rejects_with_flake() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(InitArgs {
            path: dir.path().to_path_buf(),
            name: Some("demo".into()),
            port: 3000,
            memory: "256mb".into(),
            runtime: None,
            bin: None,
            package: Some("navidrome".into()),
            with_flake: true,
            force: false,
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--package cannot be combined with --with-flake"),
            "unexpected err: {msg}"
        );
        assert!(!dir.path().join(RUSSELFILE_NAME).exists());
        assert!(!dir.path().join(FLAKE_NAME).exists());
    }
}
