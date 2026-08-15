//! Allowlisted `podman run` passthrough validation (default-deny).

use std::path::{Path, PathBuf};

/// When `RUSSEL_ALLOW_PODMAN_ARGS=0` (or `false`/`no`/`off`/`disabled`), all
/// passthrough extras are rejected. Unset or any other value leaves the
/// allowlist in effect.
fn podman_passthrough_disabled() -> bool {
    russel_core::env_util::env_bool(std::env::var("RUSSEL_ALLOW_PODMAN_ARGS").ok().as_deref())
        .map(|b| !b)
        .unwrap_or(false)
}

/// Require a following value token for a space-separated flag form.
fn require_passthrough_value<'a>(flag: &str, next: Option<&'a String>) -> anyhow::Result<&'a str> {
    match next {
        Some(v) => {
            if v.starts_with('-') {
                deny_known_unsafe_passthrough(v)?;
            }
            Ok(v.as_str())
        }
        None => anyhow::bail!("podman passthrough arg {flag} requires a value"),
    }
}

/// Allowed network modes for passthrough `--network` / `--net`.
const ALLOWED_NETWORKS: &[&str] = &["bridge", "none", "slirp4netns", "pasta"];

fn validate_passthrough_network_mode(flag: &str, val: &str) -> anyhow::Result<()> {
    if val == "host" {
        // Keep the historical "host" wording for tests and operators.
        if flag.contains('=') || flag.starts_with("--network=") || flag.starts_with("--net=") {
            anyhow::bail!("podman passthrough arg denied for security: --network=host");
        }
        anyhow::bail!("podman passthrough arg denied for security: {flag} host");
    }
    if !ALLOWED_NETWORKS.contains(&val) {
        anyhow::bail!(
            "podman passthrough arg denied for security: {flag} {val} \
             (only bridge, none, slirp4netns, pasta are permitted)"
        );
    }
    Ok(())
}

fn validate_passthrough_userns(val: &str) -> anyhow::Result<()> {
    if val != "keep-id" {
        anyhow::bail!(
            "podman passthrough arg denied for security: --userns {val} \
             (only keep-id is permitted)"
        );
    }
    Ok(())
}

/// Reject env assignments that would override Russel-managed `PORT`.
fn validate_passthrough_env_assignment(flag: &str, val: &str) -> anyhow::Result<()> {
    if val == "PORT" || val.starts_with("PORT=") {
        if val == "PORT" {
            anyhow::bail!("podman passthrough arg must not override PORT ({flag} PORT)");
        }
        anyhow::bail!("podman passthrough arg must not override PORT ({flag} PORT=...)");
    }
    Ok(())
}

/// Deny known isolation-weakening / Russel-owned flags with stable error text.
/// Returns `Ok(())` when `arg` is not a special-cased deny (caller continues
/// allowlist matching). Bails when `arg` is reserved or always-denied.
fn deny_known_unsafe_passthrough(arg: &str) -> anyhow::Result<()> {
    // ── Reserved by Russel (owned flags) ──────────────────────────────────
    if arg == "--rootfs" || arg.starts_with("--rootfs=") {
        anyhow::bail!("podman passthrough arg reserved by Russel: --rootfs");
    }
    if arg == "--name" || arg == "-n" || arg.starts_with("--name=") {
        anyhow::bail!("podman passthrough arg reserved by Russel: --name/-n");
    }
    if arg == "--replace" || arg.starts_with("--replace=") {
        anyhow::bail!("podman passthrough arg reserved by Russel: --replace");
    }
    if arg == "-d" || arg == "--detach" || arg.starts_with("--detach=") {
        anyhow::bail!("podman passthrough arg reserved by Russel: detach (-d/--detach)");
    }

    // Port publish is owned by Russel (managed -p mapping).
    if arg == "-p"
        || arg == "--publish"
        || arg == "--publish-all"
        || arg == "-P"
        || arg.starts_with("--publish=")
        || (arg.starts_with("-p") && !arg.starts_with("--"))
    {
        anyhow::bail!("podman passthrough arg denied for security: port publish ({arg})");
    }

    // ── Always-denied isolation / escape flags (explicit messages) ────────
    // All --privileged variants (including =false) — deny for simplicity.
    if arg == "--privileged" || arg.starts_with("--privileged=") {
        anyhow::bail!("podman passthrough arg denied for security: --privileged");
    }
    if arg == "--pid" || arg.starts_with("--pid=") {
        anyhow::bail!("podman passthrough arg denied for security: --pid");
    }
    if arg == "--user" || arg == "-u" || arg.starts_with("--user=") || arg.starts_with("-u=") {
        anyhow::bail!("podman passthrough arg denied for security: --user/-u");
    }
    if arg == "--entrypoint" || arg.starts_with("--entrypoint=") {
        anyhow::bail!("podman passthrough arg denied for security: --entrypoint");
    }
    if arg == "--env-file" || arg.starts_with("--env-file=") {
        anyhow::bail!("podman passthrough arg denied for security: --env-file");
    }
    if arg == "--security-opt" || arg.starts_with("--security-opt=") {
        anyhow::bail!("podman passthrough arg denied for security: --security-opt");
    }
    if arg == "--cap-add" || arg.starts_with("--cap-add=") {
        anyhow::bail!("podman passthrough arg denied for security: --cap-add");
    }
    if arg == "--device" || arg.starts_with("--device=") {
        anyhow::bail!("podman passthrough arg denied for security: --device");
    }
    if arg == "--add-device" || arg.starts_with("--add-device=") {
        anyhow::bail!("podman passthrough arg denied for security: --add-device");
    }
    // Isolation-weakening / host-control surfaces not on the allowlist.
    if arg == "--hooks-dir" || arg.starts_with("--hooks-dir=") {
        anyhow::bail!("podman passthrough arg denied for security: --hooks-dir");
    }
    if arg == "--runtime" || arg.starts_with("--runtime=") {
        anyhow::bail!("podman passthrough arg denied for security: --runtime");
    }
    if arg == "--log-driver" || arg.starts_with("--log-driver=") {
        anyhow::bail!("podman passthrough arg denied for security: --log-driver");
    }
    if arg == "--log-opt" || arg.starts_with("--log-opt=") {
        anyhow::bail!("podman passthrough arg denied for security: --log-opt");
    }
    // Disallow flipping managed read-only rootfs off via passthrough.
    if arg == "--read-only=false" || arg == "--read-only=0" || arg == "--read-only=no" {
        anyhow::bail!("podman passthrough arg denied for security: --read-only=false");
    }
    if arg == "--read-only" || arg.starts_with("--read-only=") {
        // Managed by Russel; not a passthrough knob.
        anyhow::bail!("podman passthrough arg denied for security: --read-only");
    }

    Ok(())
}

/// Validate podman passthrough args with an **allowlist** of known-safe flags
/// (Issue #191). Unknown `--*` / `-X` flags are denied by default.
///
/// Allowed (with value constraints where noted):
/// - `--network` / `--net` / `-net` — bridge | none | slirp4netns | pasta
/// - `--userns=keep-id` only
/// - `--cap-drop`, `--secret`, `-e`/`--env` (non-PORT), `--label`/`-l`,
///   `--annotation`, memory/cpu limits, `--tmpfs`, `--shm-size`, `--hostname`,
///   `--ulimit`
/// - `-v` / `--volume` / `--mount` — `/nix/store/` sources, read-only only
///
/// Set `RUSSEL_ALLOW_PODMAN_ARGS=0` to reject any non-empty extras.
pub fn validate_podman_passthrough_args(args: &[String]) -> anyhow::Result<()> {
    if !args.is_empty() && podman_passthrough_disabled() {
        anyhow::bail!(
            "podman passthrough args disabled (RUSSEL_ALLOW_PODMAN_ARGS=0); \
             unset or set to 1 to allow allowlisted extras"
        );
    }

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let next = args.get(i + 1);

        // Positional tokens are never valid in passthrough (would become COMMAND).
        if !arg.starts_with('-') {
            anyhow::bail!(
                "podman passthrough arg denied for security: unexpected positional '{arg}'"
            );
        }

        // Stable denials for reserved / known-unsafe flags (before allowlist match).
        deny_known_unsafe_passthrough(arg)?;

        // ── Allowlist: network ────────────────────────────────────────────
        if arg == "--network" || arg == "--net" || arg == "-net" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_network_mode(arg, val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--network=") {
            validate_passthrough_network_mode("--network=", val)?;
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--net=") {
            validate_passthrough_network_mode("--net=", val)?;
            i += 1;
            continue;
        }

        // ── Allowlist: userns (keep-id only) ──────────────────────────────
        if arg == "--userns" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_userns(val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--userns=") {
            // Preserve "userns=…" wording for disallowed values.
            if val != "keep-id" {
                anyhow::bail!(
                    "podman passthrough arg denied for security: --userns={val} \
                     (only keep-id is permitted)"
                );
            }
            i += 1;
            continue;
        }

        // ── Allowlist: cap-drop (further drops only; re-asserted after extras) ─
        if arg == "--cap-drop" {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if arg.starts_with("--cap-drop=") {
            i += 1;
            continue;
        }

        // ── Allowlist: secret ─────────────────────────────────────────────
        if arg == "--secret" {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if arg.starts_with("--secret=") {
            i += 1;
            continue;
        }

        // ── Allowlist: env (-e / --env), non-PORT ─────────────────────────
        // Long forms before compact `-e…` so `--env` is not misparsed.
        if arg == "--env" || arg == "-e" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_env_assignment(arg, val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--env=") {
            validate_passthrough_env_assignment("--env", val)?;
            i += 1;
            continue;
        }
        // Compact -eKEY / -eKEY=value (single-letter short opt only).
        if let Some(val) = arg.strip_prefix("-e")
            && !val.is_empty()
            && !arg.starts_with("--")
        {
            // Mirror historical compact-PORT messages.
            if val == "PORT" || val.starts_with("PORT=") {
                anyhow::bail!("podman passthrough arg must not override PORT ({arg})");
            }
            i += 1;
            continue;
        }

        // ── Allowlist: label / annotation ─────────────────────────────────
        if arg == "--label" || arg == "-l" {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if arg.starts_with("--label=") {
            i += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("-l")
            && !rest.is_empty()
            && !arg.starts_with("--")
        {
            i += 1;
            continue;
        }
        if arg == "--annotation" {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if arg.starts_with("--annotation=") {
            i += 1;
            continue;
        }

        // ── Allowlist: resource limits ────────────────────────────────────
        const RESOURCE_FLAGS: &[&str] = &[
            "--memory",
            "--memory-swap",
            "--cpus",
            "--cpu-shares",
            "--cpu-quota",
            "--cpu-period",
            "--shm-size",
            "--ulimit",
            "--hostname",
        ];
        if RESOURCE_FLAGS.contains(&arg) {
            let _ = require_passthrough_value(arg, next)?;
            i += 2;
            continue;
        }
        if RESOURCE_FLAGS
            .iter()
            .any(|f| arg.starts_with(&format!("{f}=")))
        {
            i += 1;
            continue;
        }

        // ── Allowlist: tmpfs (in-container only; path:options) ────────────
        if arg == "--tmpfs" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_tmpfs(val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--tmpfs=") {
            validate_passthrough_tmpfs(val)?;
            i += 1;
            continue;
        }

        // ── Allowlist: volume / mount (nix-store RO only) ─────────────────
        // Long forms before compact `-v…`.
        if arg == "--volume" || arg == "-v" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_volume(val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--volume=") {
            validate_passthrough_volume(val)?;
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("-v")
            && !val.is_empty()
            && !arg.starts_with("--")
        {
            validate_passthrough_volume(val)?;
            i += 1;
            continue;
        }
        if arg == "--mount" {
            let val = require_passthrough_value(arg, next)?;
            validate_passthrough_mount(val)?;
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--mount=") {
            validate_passthrough_mount(val)?;
            i += 1;
            continue;
        }

        // ── Default deny: unknown flag ────────────────────────────────────
        // Prefer a short flag name in the message for operators.
        let flag_name = arg.split('=').next().unwrap_or(arg);
        anyhow::bail!(
            "podman passthrough arg denied for security: {flag_name} is not on the allowlist"
        );
    }
    Ok(())
}

/// `--tmpfs` destinations must be absolute container paths (no host bind).
fn validate_passthrough_tmpfs(val: &str) -> anyhow::Result<()> {
    let dest = val.split_once(':').map(|(d, _)| d).unwrap_or(val);
    if dest.is_empty() || !dest.starts_with('/') {
        anyhow::bail!(
            "podman passthrough arg denied for security: --tmpfs destination \
             must be an absolute container path, got {val}"
        );
    }
    if dest.split('/').any(|seg| seg == "..") {
        anyhow::bail!(
            "podman passthrough arg denied for security: --tmpfs path must not \
             contain '..' components: {val}"
        );
    }
    Ok(())
}
/// Normalize and validate a passthrough mount/volume source path.
///
/// Requires an absolute path that lexically resolves under `/nix/store/` with
/// no `..` path components. Prefix-only checks are insufficient: paths like
/// `/nix/store/../etc/shadow` start with `/nix/store/` but escape the allowlist.
///
/// For non-existent paths (common for nix store entries not present on the
/// validating host), we manually normalize by resolving `.` and rejecting `..`
/// without filesystem access. When the path exists, we also `canonicalize` and
/// re-check the result remains under `/nix/store/`.
pub(crate) fn validate_nix_store_source(source: &str) -> anyhow::Result<()> {
    if source.is_empty() {
        anyhow::bail!("empty source path");
    }

    // Fast string-level rejection of `..` path segments (also covers odd
    // encodings that Path::components may still treat as ParentDir).
    if source.split('/').any(|seg| seg == "..") {
        anyhow::bail!("source path must not contain '..' components: {source}");
    }

    let path = Path::new(source);
    if !path.is_absolute() {
        // Relative sources can never be under /nix/store/; keep the allowlist
        // phrasing so call-site error checks stay stable.
        anyhow::bail!("source {source} is not under /nix/store/");
    }

    // Lexical normalization via Path::components — reject ParentDir, collapse
    // CurDir / repeated separators, require RootDir-rooted absolute form.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) => {
                anyhow::bail!("source path has unexpected prefix: {source}");
            }
            std::path::Component::RootDir => {
                normalized.push(std::path::Component::RootDir.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                anyhow::bail!("source path must not contain '..' components: {source}");
            }
            std::path::Component::Normal(c) => {
                normalized.push(c);
            }
        }
    }

    let normalized_str = normalized
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 source path: {source}"))?;

    // Must remain under /nix/store/<...>, not merely equal to /nix/store.
    if !normalized_str.starts_with("/nix/store/") {
        anyhow::bail!("source {source} is not under /nix/store/");
    }

    // Optional FS check: if the path exists, ensure real path still under store
    // (catches symlinks that escape /nix/store).
    if path.exists() {
        match path.canonicalize() {
            Ok(canon) => {
                let canon_str = canon
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-UTF-8 canonical path for {source}"))?;
                if !canon_str.starts_with("/nix/store/") {
                    anyhow::bail!(
                        "source {source} resolves outside /nix/store/ (canonical: {canon_str})"
                    );
                }
            }
            Err(e) => {
                anyhow::bail!("source path could not be canonicalized: {source}: {e}");
            }
        }
    }

    Ok(())
}

/// Validate a `-v` / `--volume` value: only bind mounts from /nix/store/ with
/// `:ro` are permitted.  Format: `SOURCE:DESTINATION[:OPTIONS]`.
/// Explicit `rw` is always denied even if `ro` is also listed.
fn validate_passthrough_volume(val: &str) -> anyhow::Result<()> {
    // Split on first colon to get source; the rest are dest+options.
    let (source, rest) = val.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("podman passthrough arg -v/--volume value missing ':' separator: {val}")
    })?;

    if source.is_empty() {
        anyhow::bail!("podman passthrough arg -v/--volume has empty source: {val}");
    }

    validate_nix_store_source(source).map_err(|e| {
        anyhow::anyhow!("podman passthrough arg -v/--volume denied for security: {e}")
    })?;

    // The rest contains DESTINATION[:OPTIONS] where OPTIONS is a
    // comma-separated list (e.g. "z,ro" or just "ro").
    let (_dest, options) = rest.split_once(':').unwrap_or((rest, ""));
    let opts: Vec<&str> = options
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let ro_present = opts.contains(&"ro");
    let rw_present = opts.contains(&"rw");

    if rw_present {
        anyhow::bail!(
            "podman passthrough arg -v/--volume denied for security: \
             {val} requests read-write (rw is not permitted; use :ro only)"
        );
    }

    if !ro_present {
        anyhow::bail!(
            "podman passthrough arg -v/--volume denied for security: \
             {val} is not read-only (missing :ro)"
        );
    }

    Ok(())
}

/// Validate a `--mount` value: only bind mounts from /nix/store/ with
/// `ro=true` or `readonly` are permitted.
/// Format: `type=TYPE,src=SOURCE,dst=DEST[,OPTIONS]`.
/// Explicit `rw` / `rw=true` is always denied.
fn validate_passthrough_mount(val: &str) -> anyhow::Result<()> {
    let mut mount_type = None;
    let mut source = None;
    let mut readonly = false;
    let mut readwrite = false;

    for kv in val.split(',') {
        let (key, value) = match kv.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (kv.trim(), ""),
        };
        match key {
            "type" => mount_type = Some(value),
            "src" | "source" => source = Some(value),
            "ro" | "readonly" if value.is_empty() || value == "true" || value == "1" => {
                readonly = true;
            }
            "rw" if value.is_empty() || value == "true" || value == "1" => {
                readwrite = true;
            }
            _ => {}
        }
    }

    let mount_type = mount_type.unwrap_or("bind");
    if mount_type != "bind" {
        anyhow::bail!(
            "podman passthrough arg --mount denied for security: \
             only type=bind is permitted, got type={mount_type}"
        );
    }

    let source = source.ok_or_else(|| {
        anyhow::anyhow!("podman passthrough arg --mount missing source/src: {val}")
    })?;

    if source.is_empty() {
        anyhow::bail!("podman passthrough arg --mount has empty source: {val}");
    }

    validate_nix_store_source(source)
        .map_err(|e| anyhow::anyhow!("podman passthrough arg --mount denied for security: {e}"))?;

    if readwrite {
        anyhow::bail!(
            "podman passthrough arg --mount denied for security: \
             {val} requests read-write (rw is not permitted; use ro=true/readonly)"
        );
    }

    if !readonly {
        anyhow::bail!(
            "podman passthrough arg --mount denied for security: \
             {val} is not read-only (missing ro=true or readonly)"
        );
    }

    Ok(())
}

/// Ensure podman passthrough args are only used with the container runtime.
pub fn validate_podman_args_for_runtime(
    runtime: russel_core::config::RuntimeKind,
    args: &[String],
) -> anyhow::Result<()> {
    if runtime == russel_core::config::RuntimeKind::Microvm && !args.is_empty() {
        anyhow::bail!(
            "podman passthrough args require container runtime (effective runtime is microvm)"
        );
    }
    if !args.is_empty() {
        validate_podman_passthrough_args(args)?;
    }
    Ok(())
}
