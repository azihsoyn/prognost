//! The pull request a head revision belongs to, and its Viewed
//! checkboxes — through the `gh` CLI, so the user's own login and
//! permissions apply and nothing here holds a token.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct PullRequest {
    /// GraphQL node id, what `markFileAsViewed` wants.
    pub id: String,
    pub number: u64,
    pub owner: String,
    pub name: String,
    pub url: String,
}

fn gh(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("gh")
        .current_dir(root)
        .args(args)
        .output()
        .context("cannot run gh — is it installed?")?;
    if !out.status.success() {
        bail!(
            "gh {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The open pull request whose head is `sha` (or `--pr N` when given).
pub fn find_pr(root: &Path, head_sha: Option<&str>, number: Option<u64>) -> Result<PullRequest> {
    let repo = gh(
        root,
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ],
    )?;
    let (owner, name) = repo
        .trim()
        .split_once('/')
        .map(|(o, n)| (o.to_string(), n.to_string()))
        .context("gh repo view returned no owner/name")?;
    let number = match (number, head_sha) {
        (Some(n), _) => n,
        (None, Some(sha)) => {
            let out = gh(
                root,
                &[
                    "api",
                    &format!("repos/{owner}/{name}/commits/{sha}/pulls"),
                    "-q",
                    ".[] | select(.state==\"open\") | .number",
                ],
            )?;
            out.lines()
                .next()
                .and_then(|l| l.trim().parse().ok())
                .with_context(|| format!("no open pull request has {sha} as a commit"))?
        }
        (None, None) => bail!("--pr <number> is needed when the head is the working tree"),
    };
    let out = gh(
        root,
        &[
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "id,number,url",
            "-q",
            ".id + \" \" + (.number|tostring) + \" \" + .url",
        ],
    )?;
    let mut parts = out.split_whitespace();
    let id = parts.next().context("PR has no id")?.to_string();
    let url = parts.nth(1).unwrap_or_default().to_string();
    Ok(PullRequest {
        id,
        number,
        owner,
        name,
        url,
    })
}

/// Ticks the Viewed checkbox of one file on the PR.
pub fn mark_viewed(root: &Path, pr: &PullRequest, path: &str) -> Result<()> {
    gh(
        root,
        &[
            "api",
            "graphql",
            "-f",
            "query=mutation($pr:ID!,$path:String!){markFileAsViewed(input:{pullRequestId:$pr,path:$path}){clientMutationId}}",
            "-f",
            &format!("pr={}", pr.id),
            "-f",
            &format!("path={path}"),
        ],
    )?;
    Ok(())
}

/// Clears the Viewed checkbox of one file on the PR.
pub fn unmark_viewed(root: &Path, pr: &PullRequest, path: &str) -> Result<()> {
    gh(
        root,
        &[
            "api",
            "graphql",
            "-f",
            "query=mutation($pr:ID!,$path:String!){unmarkFileAsViewed(input:{pullRequestId:$pr,path:$path}){clientMutationId}}",
            "-f",
            &format!("pr={}", pr.id),
            "-f",
            &format!("path={path}"),
        ],
    )?;
    Ok(())
}

/// The files the viewer has marked Viewed on the PR.
pub fn viewed_files(root: &Path, pr: &PullRequest) -> Result<Vec<String>> {
    let out = gh(
        root,
        &[
            "api",
            "graphql",
            "--paginate",
            "-f",
            "query=query($owner:String!,$name:String!,$number:Int!,$endCursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){files(first:100,after:$endCursor){nodes{path viewerViewedState}pageInfo{hasNextPage endCursor}}}}}",
            "-f",
            &format!("owner={}", pr.owner),
            "-f",
            &format!("name={}", pr.name),
            "-F",
            &format!("number={}", pr.number),
            "--jq",
            ".data.repository.pullRequest.files.nodes[] | select(.viewerViewedState==\"VIEWED\") | .path",
        ],
    )?;
    Ok(out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}
