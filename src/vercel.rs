use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::time::Duration;

const API_BASE: &str = "https://api.vercel.com";

#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub teams: Vec<TeamSnapshot>,
}

#[derive(Clone, Debug)]
pub struct TeamSnapshot {
    pub name: String,
    pub projects: Vec<ProjectSnapshot>,
}

#[derive(Clone, Debug)]
pub struct ProjectSnapshot {
    pub name: String,
    pub dashboard_url: String,
    pub logs_url: String,
    pub latest_deployment_url: Option<String>,
    pub production_url: Option<String>,
    pub latest_status: DeploymentStatus,
    pub latest_status_label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeploymentStatus {
    Healthy,
    Failed,
    Pending,
    Missing,
}

impl DeploymentStatus {
    pub fn indicator(&self) -> &'static str {
        "●"
    }
}

pub struct VercelClient {
    client: Client,
}

impl VercelClient {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let token = token.into();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).context("Invalid Vercel token")?,
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("vercel-bar"));

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(20))
            .build()
            .context("Failed to build Vercel HTTP client")?;

        Ok(Self { client })
    }

    pub fn validate_token(&self) -> Result<()> {
        let _: UserResponse = self.get_json("/v2/user", &[])?;
        Ok(())
    }

    pub fn fetch_snapshot(&self) -> Result<AppSnapshot> {
        self.get_json::<UserResponse>("/v2/user", &[])?;
        // Team listing can be unavailable for tokens scoped only to personal
        // projects; personal projects should still be shown in that case.
        let teams = self
            .get_json::<TeamsResponse>("/v2/teams", &[("limit", "100".to_owned())])
            .unwrap_or(TeamsResponse { teams: Vec::new() });

        let mut snapshots = Vec::new();
        for team in teams.teams {
            let name = team.name.clone().unwrap_or_else(|| team.slug.clone());
            snapshots.push(TeamSnapshot {
                name,
                projects: self.fetch_projects(Some(&team.id), &team.slug)?,
            });
        }

        snapshots.retain(|team| !team.projects.is_empty());
        snapshots.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        });

        Ok(AppSnapshot { teams: snapshots })
    }

    fn fetch_projects(
        &self,
        team_id: Option<&str>,
        scope_slug: &str,
    ) -> Result<Vec<ProjectSnapshot>> {
        let mut all_projects = Vec::new();
        let mut next: Option<i64> = None;

        loop {
            let mut query = vec![("limit", "100".to_owned())];
            if let Some(team_id) = team_id {
                query.push(("teamId", team_id.to_owned()));
            }
            if let Some(from) = next {
                query.push(("from", from.to_string()));
            }

            let response = match self.get_json::<ProjectsResponse>("/v10/projects", &query) {
                Ok(response) => response,
                Err(_) => self.get_json::<ProjectsResponse>("/v9/projects", &query)?,
            };
            match response {
                ProjectsResponse::List(projects) => {
                    all_projects.extend(projects);
                    break;
                }
                ProjectsResponse::Envelope {
                    projects,
                    pagination,
                } => {
                    if projects.is_empty() {
                        break;
                    }

                    all_projects.extend(projects);
                    match pagination.and_then(|pagination| pagination.next) {
                        Some(cursor) => next = Some(cursor),
                        None => break,
                    }
                }
            }
        }

        let mut snapshots = all_projects
            .into_iter()
            .map(|project| ProjectSnapshot::from_api(project, scope_slug))
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        });

        Ok(snapshots)
    }

    fn get_json<T>(&self, path: &str, query: &[(&str, String)]) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response = self
            .client
            .get(format!("{API_BASE}{path}"))
            .query(query)
            .send()
            .with_context(|| format!("Failed to call Vercel endpoint {path}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(anyhow!("Vercel API {path} failed with {status}: {body}"));
        }

        response
            .json()
            .with_context(|| format!("Failed to decode Vercel response from {path}"))
    }
}

impl ProjectSnapshot {
    fn from_api(project: Project, scope_slug: &str) -> Self {
        let dashboard_url = format!("https://vercel.com/{scope_slug}/{}", project.name);
        let logs_url = format!("{dashboard_url}/logs");
        let latest = project.latest_deployment();
        let latest_status = latest
            .as_ref()
            .map(Deployment::status)
            .unwrap_or(DeploymentStatus::Missing);
        let latest_status_label = latest.as_ref().map(deployment_label).unwrap_or_else(|| "Deployment: none".to_owned());
        let latest_deployment_url = latest.as_ref().and_then(|deployment| deployment.inspector_url.clone())
            .or_else(|| Some(format!("{dashboard_url}/deployments")));
        let production_url = latest.as_ref().and_then(Deployment::production_url);

        Self {
            name: project.name,
            dashboard_url,
            logs_url,
            latest_deployment_url,
            production_url,
            latest_status,
            latest_status_label,
        }

    }
}

fn deployment_label(deployment: &Deployment) -> String {
    let date = deployment.created_at.and_then(|millis| chrono::DateTime::from_timestamp_millis(millis))
        .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown date".to_owned());
    format!("Deployment: {date}")
}

#[derive(Debug, Deserialize)]
struct UserResponse {}

#[derive(Debug, Deserialize)]
struct TeamsResponse {
    teams: Vec<Team>,
}

#[derive(Debug, Deserialize)]
struct Team {
    id: String,
    slug: String,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ProjectsResponse {
    List(Vec<Project>),
    Envelope {
        projects: Vec<Project>,
        pagination: Option<Pagination>,
    },
}

#[derive(Debug, Deserialize)]
struct Pagination {
    next: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Project {
    name: String,
    #[serde(default, rename = "latestDeployments")]
    latest_deployments: Vec<Deployment>,
}

impl Project {
    fn latest_deployment(&self) -> Option<Deployment> {
        self.latest_deployments
            .iter()
            .filter(|deployment| deployment.target.as_deref() == Some("production"))
            .max_by_key(|deployment| deployment.created_at.unwrap_or_default())
            .cloned()
            .or_else(|| {
                self.latest_deployments
                    .iter()
                    .max_by_key(|deployment| deployment.created_at.unwrap_or_default())
                    .cloned()
            })
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Deployment {
    url: Option<String>,
    #[serde(default, rename = "automaticAliases")]
    automatic_aliases: Vec<String>,
    #[serde(default)]
    alias: Vec<String>,
    #[serde(rename = "aliasFinal")]
    alias_final: Option<String>,
    #[serde(rename = "inspectorUrl")]
    inspector_url: Option<String>,
    #[serde(rename = "readyState")]
    ready_state: Option<String>,
    state: Option<String>,
    target: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<i64>,
}

impl Deployment {
    fn status(&self) -> DeploymentStatus {
        match self.state_label().as_deref() {
            Some("READY") => DeploymentStatus::Healthy,
            Some("ERROR" | "BLOCKED" | "CANCELED") => DeploymentStatus::Failed,
            Some(_) => DeploymentStatus::Pending,
            None => DeploymentStatus::Missing,
        }
    }

    fn state_label(&self) -> Option<String> {
        self.ready_state
            .clone()
            .or_else(|| self.state.clone())
            .map(|state| state.to_ascii_uppercase())
    }

    fn domain(&self) -> Option<String> {
        self.alias_final
            .clone()
            .or_else(|| self.alias.first().cloned())
            .or_else(|| self.automatic_aliases.first().cloned())
            .or_else(|| self.url.clone())
    }

    fn production_url(&self) -> Option<String> {
        self.domain().map(|domain| {
            if domain.starts_with("http://") || domain.starts_with("https://") {
                domain
            } else {
                format!("https://{domain}")
            }
        })
    }
}
