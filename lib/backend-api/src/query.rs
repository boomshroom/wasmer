use std::{collections::HashSet, time::Duration};

use anyhow::{bail, Context};
use cynic::{MutationBuilder, QueryBuilder};
use edge_schema::schema::{NetworkTokenV1, WebcIdent};
use futures::StreamExt;
use time::OffsetDateTime;
use tracing::Instrument;
use url::Url;

use crate::{
    types::{
        self, CreateNamespaceVars, DeployApp, DeployAppConnection, DeployAppVersion,
        DeployAppVersionConnection, GetDeployAppAndVersion, GetDeployAppVersionsVars,
        GetNamespaceAppsVars, Log, PackageVersionConnection, PublishDeployAppVars,
    },
    GraphQLApiFailure, WasmerClient,
};

/// Load a webc package from the registry.
///
/// NOTE: this uses the public URL instead of the download URL available through
/// the API, and should not be used where possible.
pub async fn fetch_webc_package(
    client: &WasmerClient,
    ident: &WebcIdent,
    default_registry: &Url,
) -> Result<webc::compat::Container, anyhow::Error> {
    let url = ident.build_download_url_with_default_registry(default_registry);
    let data = client
        .client
        .get(url)
        .header(reqwest::header::USER_AGENT, &client.user_agent)
        .header(reqwest::header::ACCEPT, "application/webc")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    webc::compat::Container::from_bytes(data).context("failed to parse webc package")
}

/// Get the currently logged in used, together with all accessible namespaces.
///
/// You can optionally filter the namespaces by the user role.
pub async fn current_user_with_namespaces(
    client: &WasmerClient,
    namespace_role: Option<types::GrapheneRole>,
) -> Result<types::UserWithNamespaces, anyhow::Error> {
    client
        .run_graphql(types::GetCurrentUser::build(types::GetCurrentUserVars {
            namespace_role,
        }))
        .await?
        .viewer
        .context("not logged in")
}

/// Retrieve an app.
pub async fn get_app(
    client: &WasmerClient,
    owner: String,
    name: String,
) -> Result<Option<types::DeployApp>, anyhow::Error> {
    client
        .run_graphql(types::GetDeployApp::build(types::GetDeployAppVars {
            name,
            owner,
        }))
        .await
        .map(|x| x.get_deploy_app)
}

/// Retrieve an app by its global alias.
pub async fn get_app_by_alias(
    client: &WasmerClient,
    alias: String,
) -> Result<Option<types::DeployApp>, anyhow::Error> {
    client
        .run_graphql(types::GetDeployAppByAlias::build(
            types::GetDeployAppByAliasVars { alias },
        ))
        .await
        .map(|x| x.get_app_by_global_alias)
}

/// Retrieve an app version.
pub async fn get_app_version(
    client: &WasmerClient,
    owner: String,
    name: String,
    version: String,
) -> Result<Option<types::DeployAppVersion>, anyhow::Error> {
    client
        .run_graphql(types::GetDeployAppVersion::build(
            types::GetDeployAppVersionVars {
                name,
                owner,
                version,
            },
        ))
        .await
        .map(|x| x.get_deploy_app_version)
}

/// Retrieve an app together with a specific version.
pub async fn get_app_with_version(
    client: &WasmerClient,
    owner: String,
    name: String,
    version: String,
) -> Result<GetDeployAppAndVersion, anyhow::Error> {
    client
        .run_graphql(types::GetDeployAppAndVersion::build(
            types::GetDeployAppAndVersionVars {
                name,
                owner,
                version,
            },
        ))
        .await
}

/// Retrieve an app together with a specific version.
pub async fn get_app_and_package_by_name(
    client: &WasmerClient,
    vars: types::GetPackageAndAppVars,
) -> Result<(Option<types::Package>, Option<types::DeployApp>), anyhow::Error> {
    let res = client
        .run_graphql(types::GetPackageAndApp::build(vars))
        .await?;
    Ok((res.get_package, res.get_deploy_app))
}

/// Retrieve apps.
pub async fn get_deploy_apps(
    client: &WasmerClient,
    vars: types::GetDeployAppsVars,
) -> Result<DeployAppConnection, anyhow::Error> {
    let res = client
        .run_graphql(types::GetDeployApps::build(vars))
        .await?;
    res.get_deploy_apps.context("no apps returned")
}

/// Retrieve apps as a stream that will automatically paginate.
pub fn get_deploy_apps_stream(
    client: &WasmerClient,
    vars: types::GetDeployAppsVars,
) -> impl futures::Stream<Item = Result<Vec<DeployApp>, anyhow::Error>> + '_ {
    futures::stream::try_unfold(
        Some(vars),
        move |vars: Option<types::GetDeployAppsVars>| async move {
            let vars = match vars {
                Some(vars) => vars,
                None => return Ok(None),
            };

            let page = get_deploy_apps(client, vars.clone()).await?;

            let end_cursor = page.page_info.end_cursor;

            let items = page
                .edges
                .into_iter()
                .filter_map(|x| x.and_then(|x| x.node))
                .collect::<Vec<_>>();

            let new_vars = end_cursor.map(|c| types::GetDeployAppsVars {
                after: Some(c),
                ..vars
            });

            Ok(Some((items, new_vars)))
        },
    )
}

/// Retrieve versions for an app.
pub async fn get_deploy_app_versions(
    client: &WasmerClient,
    vars: GetDeployAppVersionsVars,
) -> Result<DeployAppVersionConnection, anyhow::Error> {
    let res = client
        .run_graphql_strict(types::GetDeployAppVersions::build(vars))
        .await?;
    let versions = res.get_deploy_app.context("app not found")?.versions;
    Ok(versions)
}

/// Load all versions of an app.
///
/// Will paginate through all versions and return them in a single list.
pub async fn all_app_versions(
    client: &WasmerClient,
    owner: String,
    name: String,
) -> Result<Vec<DeployAppVersion>, anyhow::Error> {
    let mut vars = GetDeployAppVersionsVars {
        owner,
        name,
        offset: None,
        before: None,
        after: None,
        first: Some(10),
        last: None,
        sort_by: None,
    };

    let mut all_versions = Vec::<DeployAppVersion>::new();

    loop {
        let page = get_deploy_app_versions(client, vars.clone()).await?;
        dbg!(&page);
        if page.edges.is_empty() {
            break;
        }

        for edge in page.edges {
            let edge = match edge {
                Some(edge) => edge,
                None => continue,
            };
            let version = match edge.node {
                Some(item) => item,
                None => continue,
            };

            // Sanity check to avoid duplication.
            if all_versions.iter().any(|v| v.id == version.id) == false {
                all_versions.push(version);
            }

            // Update pagination.
            vars.after = Some(edge.cursor);
        }
    }

    Ok(all_versions)
}

/// Activate a particular version of an app.
pub async fn app_version_activate(
    client: &WasmerClient,
    version: String,
) -> Result<DeployApp, anyhow::Error> {
    let res = client
        .run_graphql_strict(types::MarkAppVersionAsActive::build(
            types::MarkAppVersionAsActiveVars {
                input: types::MarkAppVersionAsActiveInput {
                    app_version: version.into(),
                },
            },
        ))
        .await?;
    res.mark_app_version_as_active
        .context("app not found")
        .map(|x| x.app)
}

/// Retrieve a node based on its global id.
pub async fn get_node(
    client: &WasmerClient,
    id: String,
) -> Result<Option<types::Node>, anyhow::Error> {
    client
        .run_graphql(types::GetNode::build(types::GetNodeVars { id: id.into() }))
        .await
        .map(|x| x.node)
}

/// Retrieve an app by its global id.
pub async fn get_app_by_id(
    client: &WasmerClient,
    app_id: String,
) -> Result<DeployApp, anyhow::Error> {
    client
        .run_graphql(types::GetDeployAppById::build(
            types::GetDeployAppByIdVars {
                app_id: app_id.into(),
            },
        ))
        .await?
        .app
        .context("app not found")?
        .into_deploy_app()
        .context("app conversion failed")
}

/// Retrieve an app together with a specific version.
pub async fn get_app_with_version_by_id(
    client: &WasmerClient,
    app_id: String,
    version_id: String,
) -> Result<(DeployApp, DeployAppVersion), anyhow::Error> {
    let res = client
        .run_graphql(types::GetDeployAppAndVersionById::build(
            types::GetDeployAppAndVersionByIdVars {
                app_id: app_id.into(),
                version_id: version_id.into(),
            },
        ))
        .await?;

    let app = res
        .app
        .context("app not found")?
        .into_deploy_app()
        .context("app conversion failed")?;
    let version = res
        .version
        .context("version not found")?
        .into_deploy_app_version()
        .context("version conversion failed")?;

    Ok((app, version))
}

/// Retrieve an app version by its global id.
pub async fn get_app_version_by_id(
    client: &WasmerClient,
    version_id: String,
) -> Result<DeployAppVersion, anyhow::Error> {
    client
        .run_graphql(types::GetDeployAppVersionById::build(
            types::GetDeployAppVersionByIdVars {
                version_id: version_id.into(),
            },
        ))
        .await?
        .version
        .context("app not found")?
        .into_deploy_app_version()
        .context("app version conversion failed")
}

pub async fn get_app_version_by_id_with_app(
    client: &WasmerClient,
    version_id: String,
) -> Result<(DeployApp, DeployAppVersion), anyhow::Error> {
    let version = client
        .run_graphql(types::GetDeployAppVersionById::build(
            types::GetDeployAppVersionByIdVars {
                version_id: version_id.into(),
            },
        ))
        .await?
        .version
        .context("app not found")?
        .into_deploy_app_version()
        .context("app version conversion failed")?;

    let app_id = version
        .app
        .as_ref()
        .context("could not load app for version")?
        .id
        .clone();

    let app = get_app_by_id(client, app_id.into_inner()).await?;

    Ok((app, version))
}

/// List all apps that are accessible by the current user.
///
/// NOTE: this will only include the first pages and does not provide pagination.
pub async fn user_apps(client: &WasmerClient) -> Result<Vec<types::DeployApp>, anyhow::Error> {
    let user = client
        .run_graphql(types::GetCurrentUserWithApps::build(()))
        .await?
        .viewer
        .context("not logged in")?;

    let apps = user
        .apps
        .edges
        .into_iter()
        .flatten()
        .filter_map(|x| x.node)
        .collect();

    Ok(apps)
}

/// List all apps that are accessible by the current user.
///
/// NOTE: this does not currently do full pagination properly.
// TODO(theduke): fix pagination
pub async fn user_accessible_apps(
    client: &WasmerClient,
) -> Result<Vec<types::DeployApp>, anyhow::Error> {
    let mut apps = Vec::new();

    // Get user apps.

    let user_apps = user_apps(client).await?;

    apps.extend(user_apps);

    // Get all aps in user-accessible namespaces.
    let namespace_res = client
        .run_graphql(types::GetCurrentUser::build(types::GetCurrentUserVars {
            namespace_role: None,
        }))
        .await?;
    let active_user = namespace_res.viewer.context("not logged in")?;
    let namespace_names = active_user
        .namespaces
        .edges
        .iter()
        .filter_map(|edge| edge.as_ref())
        .filter_map(|edge| edge.node.as_ref())
        .map(|node| node.name.clone())
        .collect::<Vec<_>>();

    for namespace in namespace_names {
        let out = client
            .run_graphql(types::GetNamespaceApps::build(GetNamespaceAppsVars {
                name: namespace.to_string(),
            }))
            .await?;

        if let Some(ns) = out.get_namespace {
            let ns_apps = ns.apps.edges.into_iter().flatten().filter_map(|x| x.node);
            apps.extend(ns_apps);
        }
    }
    Ok(apps)
}

/// Get apps for a specific namespace.
///
/// NOTE: only retrieves the first page and does not do pagination.
pub async fn namespace_apps(
    client: &WasmerClient,
    namespace: &str,
) -> Result<Vec<types::DeployApp>, anyhow::Error> {
    let res = client
        .run_graphql(types::GetNamespaceApps::build(GetNamespaceAppsVars {
            name: namespace.to_string(),
        }))
        .await?;

    let ns = res
        .get_namespace
        .with_context(|| format!("failed to get namespace '{}'", namespace))?;

    let apps = ns
        .apps
        .edges
        .into_iter()
        .flatten()
        .filter_map(|x| x.node)
        .collect();

    Ok(apps)
}

/// Publish a new app (version).
pub async fn publish_deploy_app(
    client: &WasmerClient,
    vars: PublishDeployAppVars,
) -> Result<DeployAppVersion, anyhow::Error> {
    let res = client
        .run_graphql_raw(types::PublishDeployApp::build(vars))
        .await?;

    if let Some(app) = res
        .data
        .and_then(|d| d.publish_deploy_app)
        .map(|d| d.deploy_app_version)
    {
        Ok(app)
    } else {
        Err(GraphQLApiFailure::from_errors(
            "could not publish app",
            res.errors,
        ))
    }
}

/// Delete an app.
pub async fn delete_app(client: &WasmerClient, app_id: String) -> Result<(), anyhow::Error> {
    let res = client
        .run_graphql_strict(types::DeleteApp::build(types::DeleteAppVars {
            app_id: app_id.into(),
        }))
        .await?
        .delete_app
        .context("API did not return data for the delete_app mutation")?;

    if !res.success {
        bail!("App deletion failed for an unknown reason");
    }

    Ok(())
}

/// Get all namespaces accessible by the current user.
pub async fn user_namespaces(
    client: &WasmerClient,
) -> Result<Vec<types::Namespace>, anyhow::Error> {
    let user = client
        .run_graphql(types::GetCurrentUser::build(types::GetCurrentUserVars {
            namespace_role: None,
        }))
        .await?
        .viewer
        .context("not logged in")?;

    let ns = user
        .namespaces
        .edges
        .into_iter()
        .flatten()
        // .filter_map(|x| x)
        .filter_map(|x| x.node)
        .collect();

    Ok(ns)
}

/// Retrieve a namespace by its name.
pub async fn get_namespace(
    client: &WasmerClient,
    name: String,
) -> Result<Option<types::Namespace>, anyhow::Error> {
    client
        .run_graphql(types::GetNamespace::build(types::GetNamespaceVars { name }))
        .await
        .map(|x| x.get_namespace)
}

/// Create a new namespace.
pub async fn create_namespace(
    client: &WasmerClient,
    vars: CreateNamespaceVars,
) -> Result<types::Namespace, anyhow::Error> {
    client
        .run_graphql(types::CreateNamespace::build(vars))
        .await?
        .create_namespace
        .map(|x| x.namespace)
        .context("no namespace returned")
}

/// Retrieve a package by its name.
pub async fn get_package(
    client: &WasmerClient,
    name: String,
) -> Result<Option<types::Package>, anyhow::Error> {
    client
        .run_graphql_strict(types::GetPackage::build(types::GetPackageVars { name }))
        .await
        .map(|x| x.get_package)
}

/// Retrieve a package version by its name.
pub async fn get_package_version(
    client: &WasmerClient,
    name: String,
    version: String,
) -> Result<Option<types::PackageVersionWithPackage>, anyhow::Error> {
    client
        .run_graphql_strict(types::GetPackageVersion::build(
            types::GetPackageVersionVars { name, version },
        ))
        .await
        .map(|x| x.get_package_version)
}

/// Retrieve package versions for an app.
pub async fn get_package_versions(
    client: &WasmerClient,
    vars: types::AllPackageVersionsVars,
) -> Result<PackageVersionConnection, anyhow::Error> {
    let res = client
        .run_graphql(types::GetAllPackageVersions::build(vars))
        .await?;
    Ok(res.all_package_versions)
}

/// Retrieve all versions of a package as a stream that auto-paginates.
pub fn get_package_versions_stream(
    client: &WasmerClient,
    vars: types::AllPackageVersionsVars,
) -> impl futures::Stream<Item = Result<Vec<types::PackageVersionWithPackage>, anyhow::Error>> + '_
{
    futures::stream::try_unfold(
        Some(vars),
        move |vars: Option<types::AllPackageVersionsVars>| async move {
            let vars = match vars {
                Some(vars) => vars,
                None => return Ok(None),
            };

            let page = get_package_versions(client, vars.clone()).await?;

            let end_cursor = page.page_info.end_cursor;

            let items = page
                .edges
                .into_iter()
                .filter_map(|x| x.and_then(|x| x.node))
                .collect::<Vec<_>>();

            let new_vars = end_cursor.map(|cursor| types::AllPackageVersionsVars {
                after: Some(cursor),
                ..vars
            });

            Ok(Some((items, new_vars)))
        },
    )
}

/// Generate a new Edge token.
pub async fn generate_deploy_token_raw(
    client: &WasmerClient,
    app_version_id: String,
) -> Result<String, anyhow::Error> {
    let res = client
        .run_graphql(types::GenerateDeployToken::build(
            types::GenerateDeployTokenVars { app_version_id },
        ))
        .await?;

    res.generate_deploy_token
        .map(|x| x.token)
        .context("no token returned")
}

#[derive(Debug, PartialEq)]
pub enum GenerateTokenBy {
    Id(NetworkTokenV1),
}

#[derive(Debug, PartialEq)]
pub enum TokenKind {
    SSH,
    Network(GenerateTokenBy),
}

pub async fn generate_deploy_config_token_raw(
    client: &WasmerClient,
    token_kind: TokenKind,
) -> Result<String, anyhow::Error> {
    let res = client
        .run_graphql(types::GenerateDeployConfigToken::build(
            types::GenerateDeployConfigTokenVars {
                input: match token_kind {
                    TokenKind::SSH => "{}".to_string(),
                    TokenKind::Network(by) => match by {
                        GenerateTokenBy::Id(token) => serde_json::to_string(&token)?,
                    },
                },
            },
        ))
        .await?;

    res.generate_deploy_config_token
        .map(|x| x.token)
        .context("no token returned")
}

/// Get pages of logs associated with an application that lie within the
/// specified date range.
// NOTE: this is not public due to severe usability issues.
// The stream can loop forever due to re-fetching the same logs over and over.
#[tracing::instrument(skip_all, level = "debug")]
#[allow(clippy::let_with_type_underscore)]
fn get_app_logs(
    client: &WasmerClient,
    name: String,
    owner: String,
    tag: Option<String>,
    start: OffsetDateTime,
    end: Option<OffsetDateTime>,
    watch: bool,
) -> impl futures::Stream<Item = Result<Vec<Log>, anyhow::Error>> + '_ {
    // Note: the backend will limit responses to a certain number of log
    // messages, so we use try_unfold() to keep calling it until we stop getting
    // new log messages.
    let span = tracing::Span::current();

    futures::stream::try_unfold(start, move |start| {
        let variables = types::GetDeployAppLogsVars {
            name: name.clone(),
            owner: owner.clone(),
            version: tag.clone(),
            // TODO: increase pagination size
            // See https://github.com/wasmerio/edge/issues/460
            // first: Some(500),
            first: Some(10),
            starting_from: unix_timestamp(start),
            until: end.map(unix_timestamp),
        };

        let fut = async move {
            loop {
                let deploy_app_version = client
                    .run_graphql(types::GetDeployAppLogs::build(variables.clone()))
                    .await?
                    .get_deploy_app_version
                    .context("unknown package version")?;

                let page: Vec<_> = deploy_app_version
                    .logs
                    .edges
                    .into_iter()
                    .flatten()
                    .filter_map(|edge| edge.node)
                    .collect();

                if page.is_empty() {
                    if watch {
                        /*
                            TODO: the resolution of watch should be configurable
                            TODO: should this be async?
                        */
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }

                    break Ok(None);
                } else {
                    let last_message = page.last().expect("The page is non-empty");
                    let timestamp = last_message.timestamp;
                    // NOTE: adding 1 microsecond to the timestamp to avoid fetching
                    // the last message again.
                    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(timestamp as i128)
                        .with_context(|| {
                            format!("Unable to interpret {timestamp} as a unix timestamp")
                        })?;

                    // FIXME: We need a better way to tell the backend "give me the
                    // next set of logs". Adding 1 nanosecond could theoretically
                    // mean we miss messages if multiple log messages arrived at
                    // the same nanosecond and the page ended midway.

                    let next_timestamp = timestamp + Duration::from_nanos(1_000);

                    break Ok(Some((page, next_timestamp)));
                }
            }
        };

        fut.instrument(span.clone())
    })
}

/// Get pages of logs associated with an application that lie within the
/// specified date range.
///
/// In contrast to [`get_app_logs`], this function collects the stream into a
/// final vector.
#[tracing::instrument(skip_all, level = "debug")]
#[allow(clippy::let_with_type_underscore)]
pub async fn get_app_logs_paginated(
    client: &WasmerClient,
    name: String,
    owner: String,
    tag: Option<String>,
    start: OffsetDateTime,
    end: Option<OffsetDateTime>,
    watch: bool,
) -> impl futures::Stream<Item = Result<Vec<Log>, anyhow::Error>> + '_ {
    let stream = get_app_logs(client, name, owner, tag, start, end, watch);

    stream.map(|res| {
        let mut logs = Vec::new();
        let mut hasher = HashSet::new();
        let mut page = res?;

        // Prevent duplicates.
        // TODO: don't clone the message, just hash it.
        page.retain(|log| hasher.insert((log.message.clone(), log.timestamp.round() as i128)));

        logs.extend(page);

        Ok(logs)
    })
}

/// Convert a [`OffsetDateTime`] to a unix timestamp that the WAPM backend
/// understands.
fn unix_timestamp(ts: OffsetDateTime) -> f64 {
    let nanos_per_second = 1_000_000_000;
    let timestamp = ts.unix_timestamp_nanos();
    let nanos = timestamp % nanos_per_second;
    let secs = timestamp / nanos_per_second;

    (secs as f64) + (nanos as f64 / nanos_per_second as f64)
}
