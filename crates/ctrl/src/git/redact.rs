//! Strip HTTP(S) userinfo from repository URLs before log or persist.

/// Return a copy of `url` safe to log or write to metadata.
///
/// For `http://` and `https://`, userinfo (`user:token@`) is removed so the
/// stored form is `https://host/path`. `ssh://` keeps a username but drops
/// userinfo that includes a colon (password-shaped). SCP `git@host:path` and
/// local paths are left unchanged. Garbage input never panics: the original
/// string is returned when the value is not a URL we know how to rewrite.
pub fn redact_repo_url(url: &str) -> String {
    match strip_userinfo(url) {
        Some((scheme, _userinfo, hostport, after)) => format!("{scheme}{hostport}{after}"),
        None => url.to_string(),
    }
}

/// Rewrite `text` so neither the raw `repo` URL nor its userinfo remain.
///
/// Git/libcurl sometimes echo the URL with a different host case than the
/// request string, so replacing `repo` alone is not enough.
pub(super) fn scrub_logged_repo_text(text: &str, repo: &str) -> String {
    let safe = redact_repo_url(repo);
    let mut out = text.to_string();
    if !repo.is_empty() && repo != safe {
        out = out.replace(repo, &safe);
    }
    if let Some((_, userinfo, _, _)) = strip_userinfo(repo) {
        let needle = format!("{userinfo}@");
        if !needle.is_empty() {
            out = out.replace(&needle, "");
        }
    }
    out
}

fn strip_userinfo(url: &str) -> Option<(&str, &str, &str, &str)> {
    let lower = url.to_ascii_lowercase();
    let prefix_len = if lower.starts_with("https://") {
        8
    } else if lower.starts_with("http://") {
        7
    } else if lower.starts_with("ssh://") {
        6
    } else {
        return None;
    };

    let scheme = &url[..prefix_len];
    let rest = &url[prefix_len..];
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let after = &rest[auth_end..];
    if authority.is_empty() {
        return None;
    }
    let (userinfo, hostport) = authority.rsplit_once('@')?;
    if userinfo.is_empty() {
        return None;
    }
    // ssh://user@host is a login name, not a password. Only rewrite when the
    // userinfo clearly embeds a password (`user:pass@`).
    if scheme.eq_ignore_ascii_case("ssh://") && !userinfo.contains(':') {
        return None;
    }
    Some((scheme, userinfo, hostport, after))
}
