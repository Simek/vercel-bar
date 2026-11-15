use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const API_BASE: &str = "https://api.vercel.com";
const MAX_CONCURRENT_REQUESTS: usize = 8;

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
    pub latest_deployment_id: Option<String>,
    pub team_id: Option<String>,
    pub page_views: Option<u64>,
    project_id: String,
    latest_deployment_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeploymentStatus {
    Healthy,
    Failed,
    Pending,
    Missing,
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

    pub fn redeploy(
        &self,
        deployment_id: &str,
        project_name: &str,
        team_id: Option<&str>,
    ) -> Result<()> {
        let mut query = vec![("forceNew", "1".to_owned())];
        if let Some(team_id) = team_id {
            query.push(("teamId", team_id.to_owned()));
        }
        let body = serde_json::json!({
            "deploymentId": deployment_id,
            "meta": { "action": "redeploy" },
            "name": project_name,
            "target": "production",
        });
        self.post_json_body("/v13/deployments", &query, &body)
            .map(|_: serde_json::Value| ())
    }

    pub fn fetch_snapshot(&self) -> Result<AppSnapshot> {
        self.fetch_snapshot_with_page_views_filter(|_, _| true)
    }

    pub fn fetch_snapshot_with_page_views_filter(
        &self,
        should_fetch_page_views: impl Fn(&str, &str) -> bool,
    ) -> Result<AppSnapshot> {
        self.get_json::<UserResponse>("/v2/user", &[])?;
        // Team listing can be unavailable for tokens scoped only to personal
        // projects; personal projects should still be shown in that case.
        let teams = self
            .get_json::<TeamsResponse>("/v2/teams", &[("limit", "100".to_owned())])
            .unwrap_or(TeamsResponse { teams: Vec::new() });

        let mut snapshots = map_concurrently(&teams.teams, |team| {
            let name = team.name.clone().unwrap_or_else(|| team.slug.clone());
            self.fetch_projects(Some(&team.id), &team.slug)
                .map(|projects| (TeamSnapshot { name, projects }, team.slug.clone()))
        })
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

        snapshots.retain(|(team, _)| !team.projects.is_empty());
        snapshots.sort_by(|(left, _), (right, _)| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        });

        let mut page_view_requests = Vec::new();
        for (team_index, (team, team_slug)) in snapshots.iter().enumerate() {
            for (project_index, project) in team.projects.iter().enumerate() {
                if should_fetch_page_views(&team.name, &project.name) {
                    page_view_requests.push(PageViewsRequest {
                        team_index,
                        project_index,
                        project_id: project.project_id.clone(),
                        team_id: project.team_id.clone(),
                        team_slug: team_slug.clone(),
                    });
                }
            }
        }

        let page_views = map_concurrently(&page_view_requests, |request| {
            self.fetch_page_views(
                &request.project_id,
                request.team_id.as_deref(),
                Some(&request.team_slug),
            )
            .ok()
        });
        for (request, page_views) in page_view_requests.iter().zip(page_views) {
            snapshots[request.team_index].0.projects[request.project_index].page_views = page_views;
        }

        Ok(AppSnapshot {
            teams: snapshots.into_iter().map(|(team, _)| team).collect(),
        })
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
            .map(|project| ProjectSnapshot::from_api(project, scope_slug, None, team_id))
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            right
                .latest_deployment_at
                .cmp(&left.latest_deployment_at)
                .then_with(|| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                })
        });

        Ok(snapshots)
    }

    fn fetch_page_views(
        &self,
        project_id: &str,
        team_id: Option<&str>,
        team_slug: Option<&str>,
    ) -> Result<u64> {
        let until = chrono::Utc::now();
        let since = until - chrono::Duration::days(7);
        let mut query = vec![
            ("projectId", project_id.to_owned()),
            ("since", since.timestamp_millis().to_string()),
            ("until", until.timestamp_millis().to_string()),
        ];
        if let Some(team_id) = team_id {
            query.push(("teamId", team_id.to_owned()));
        }
        if let Some(team_slug) = team_slug {
            query.push(("slug", team_slug.to_owned()));
        }
        let response: PageViewsResponse =
            self.get_json("/v1/query/web-analytics/visits/count", &query)?;
        Ok(response.data.pageviews)
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

    fn post_json_body<T>(
        &self,
        path: &str,
        query: &[(&str, String)],
        body: &serde_json::Value,
    ) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response = self
            .client
            .post(format!("{API_BASE}{path}"))
            .query(query)
            .json(body)
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

struct PageViewsRequest {
    team_index: usize,
    project_index: usize,
    project_id: String,
    team_id: Option<String>,
    team_slug: String,
}

fn map_concurrently<T, R>(items: &[T], map: impl Fn(&T) -> R + Sync) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    if items.is_empty() {
        return Vec::new();
    }

    let next_index = AtomicUsize::new(0);
    let results = Mutex::new(
        std::iter::repeat_with(|| None)
            .take(items.len())
            .collect::<Vec<Option<R>>>(),
    );
    std::thread::scope(|scope| {
        for _ in 0..items.len().min(MAX_CONCURRENT_REQUESTS) {
            let map = &map;
            let next_index = &next_index;
            let results = &results;
            scope.spawn(move || {
                loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = map(item);
                    results.lock().unwrap()[index] = Some(result);
                }
            });
        }
    });

    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|result| result.expect("concurrent worker did not produce a result"))
        .collect()
}

impl ProjectSnapshot {
    pub fn latest_deployment_at(&self) -> Option<i64> {
        self.latest_deployment_at
    }

    fn from_api(
        project: Project,
        scope_slug: &str,
        page_views: Option<u64>,
        team_id: Option<&str>,
    ) -> Self {
        let dashboard_url = format!("https://vercel.com/{scope_slug}/{}", project.name);
        let logs_url = format!("{dashboard_url}/logs");
        let latest = project.latest_deployment();
        let latest_status = latest
            .as_ref()
            .map(Deployment::status)
            .unwrap_or(DeploymentStatus::Missing);
        let latest_status_label = latest
            .as_ref()
            .map(deployment_label)
            .unwrap_or_else(|| "Deployment: none".to_owned());
        let latest_deployment_url = latest
            .as_ref()
            .and_then(|deployment| deployment.inspector_url.clone())
            .or_else(|| {
                latest
                    .as_ref()
                    .and_then(|deployment| deployment.uid.as_deref().or(deployment.id.as_deref()))
                    .map(|deployment_id| format!("{dashboard_url}/{deployment_id}"))
            });
        let production_url = latest.as_ref().and_then(Deployment::production_url);
        let project_id = project.id.clone().unwrap_or_else(|| project.name.clone());

        Self {
            name: project.name,
            dashboard_url,
            logs_url,
            latest_deployment_url,
            production_url,
            latest_status,
            latest_status_label,
            latest_deployment_id: latest.as_ref().and_then(|deployment| {
                (deployment.target.as_deref() == Some("production"))
                    .then(|| deployment.uid.clone().or(deployment.id.clone()))
                    .flatten()
            }),
            team_id: team_id.map(str::to_owned),
            page_views,
            project_id,
            latest_deployment_at: latest.as_ref().and_then(|deployment| deployment.created_at),
        }
    }
}

fn deployment_label(deployment: &Deployment) -> String {
    let date = deployment
        .created_at
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| {
            let age = chrono::Utc::now().signed_duration_since(time);
            let relative = if age.num_minutes() < 1 {
                "less than a minute".to_owned()
            } else if age.num_hours() < 1 {
                format!("{}min", age.num_minutes())
            } else if age.num_days() < 1 {
                format!("{}h", age.num_hours())
            } else {
                format!("{}d", age.num_days())
            };
            format!("{} ({} ago)", time.format("%Y-%m-%d %H:%M"), relative)
        })
        .unwrap_or_else(|| "unknown date".to_owned());
    format!("{date}")
}

#[derive(Debug, Deserialize)]
struct UserResponse {}

#[derive(Debug, Deserialize)]
struct PageViewsResponse {
    data: PageViewsData,
}

#[derive(Debug, Deserialize)]
struct PageViewsData {
    pageviews: u64,
}

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
    id: Option<String>,
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
    uid: Option<String>,
    id: Option<String>,
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
