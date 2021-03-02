use futures::{StreamExt, TryStreamExt};
use k8s_openapi as k8s;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use krator::OperatorRuntime;
use kube::api;
use kube::api::{PostParams, WatchEvent};
use kube::config;
use noqnoqnoq::self_service::{project, Sample};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::select;
use tokio::task::JoinHandle;
use tokio::time;

pub async fn before_each() -> anyhow::Result<kube::Client> {
    let kubeconfig = config::Kubeconfig::read_from(Path::new("./kind.kubeconfig"))?;
    let mut config =
        config::Config::from_custom_kubeconfig(kubeconfig, &config::KubeConfigOptions::default())
            .await?;
    config.timeout = Some(Duration::from_secs(1));

    let client = kube::Client::new(config.clone());

    // basic check so we fail early if k8s communication does not work
    assert!(
        client.apiserver_version().await.is_ok(),
        "communication with kubernetes should work"
    );

    // there is probably a better way FnOnce?
    let _ = reinstall_self_service_crd(&client).await?;

    let mut runtime =
        OperatorRuntime::new(&config, project::ProjectOperator::new(client.clone()), None);

    tokio::spawn(async move { runtime.start().await });

    Ok(client)
}

#[derive(Debug)]
pub enum WaitForState {
    Deleted,
    Created,
}

pub async fn reinstall_self_service_crd(client: &kube::Client) -> anyhow::Result<()> {
    let api: kube::Api<CustomResourceDefinition> = kube::Api::all(client.clone());
    let name = project::Project::crd().metadata.name.unwrap();

    match api.get(&name).await {
        Ok(_) => {
            let wait_for_crd_deleted = wait_for_state(&api, &name, WaitForState::Deleted);

            api.delete(&name, &api::DeleteParams::default()).await?;
            let _ = wait_for_crd_deleted.await?;
        }
        _ => {}
    }

    noqnoqnoq::helper::install_crd(&client, &project::Project::crd()).await?;

    // TODO: check ... k8s api reports delete event before it can accept resources of this type
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    Ok(())
}

pub fn wait_for_state<K: 'static>(
    api: &kube::Api<K>,
    name: &String,
    state: WaitForState,
) -> JoinHandle<()>
where
    K: k8s::Resource
        + Clone
        + std::marker::Send
        + kube::api::Meta
        + for<'de> serde::de::Deserialize<'de>
        + Sync,
{
    let name = name.clone();
    let api = api.clone();

    tokio::spawn(async move {
        println!(
            "{} with name {} waiting for state {:?}",
            K::KIND,
            name,
            state
        );

        let lp = &api::ListParams::default()
            .timeout(10)
            .fields(format!("metadata.name={}", name).as_str());

        let resource_version = api
            .list(&lp)
            .await
            .unwrap()
            .metadata
            .resource_version
            .unwrap();

        let mut stream = api.watch(lp, &resource_version).await.unwrap().boxed();

        let print_info = {
            |e: &WatchEvent<K>, resource: &K| {
                println!(
                    "  - {:?} for {} with name {} received",
                    e.clone(),
                    k8s::kind(resource),
                    resource.name()
                );
            }
        };

        // Observe the pods phase for 10 seconds
        while let Some(status) = stream.try_next().await.unwrap() {
            match status.clone() {
                WatchEvent::Added(resource) => {
                    print_info(&status, &resource);
                    if let WaitForState::Created = state {
                        break;
                    }
                }
                WatchEvent::Bookmark(bookmark) => {
                    println!(" - {:?} for {}", status, bookmark.types.kind);
                }
                WatchEvent::Modified(resource) => {
                    print_info(&status, &resource);
                }
                WatchEvent::Deleted(resource) => {
                    print_info(&status, &resource);
                    if let WaitForState::Deleted = state {
                        break;
                    }
                }
                WatchEvent::Error(e) => {
                    println!(" - ERROR watching {} with name {}: {}", K::KIND, name, e);
                }
            }
        }
    })
}

pub async fn install_project(
    client: &kube::Client,
    prefix: &str,
) -> anyhow::Result<(project::Project, String)> {
    let timeout_secs = 10;
    let name = random_name(prefix);
    let project_api: kube::Api<project::Project> = kube::Api::all(client.clone());

    let wait_for_namespace_created_handle = wait_for_state(
        &kube::Api::<Namespace>::all(client.clone()),
        &name,
        WaitForState::Created,
    );

    let wait_for_project_created_handle =
        wait_for_state(&project_api, &name, WaitForState::Created);

    let project = project::Project::new(name.as_str(), project::ProjectSpec::sample());
    let project_resource = project_api.create(&PostParams::default(), &project).await;

    assert!(
        project_resource.is_ok(),
        "creating a new self service project should work correclty"
    );
    assert!(
        select! {
        res = futures::future::try_join(wait_for_namespace_created_handle, wait_for_project_created_handle) => res.is_ok(),
            _ = time::sleep(Duration::from_secs(timeout_secs)) => false,
        },
        "expected project related namespace {} to be created within {} seconds",
        name,
        timeout_secs
    );

    Ok((project_resource.unwrap(), name))
}

pub fn random_name(prefix: &str) -> String {
    format!(
        "{}-{}",
        prefix,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    )
}

pub fn is_owned_by_project<R>(project: &project::Project, resource: &R) -> anyhow::Result<()>
where
    R: k8s_openapi::Resource + kube::api::Meta,
{
    assert!(
        resource.meta().owner_references.is_some(),
        "{} should have owner reference",
        R::KIND
    );

    let owners = resource.meta().owner_references.as_ref().unwrap();
    assert!(
        owners.len() > 0,
        "{} should have at least one owner",
        R::KIND
    );

    let owner = &owners[0];
    assert_eq!(
        owner.api_version, project.api_version,
        "api_version of owner-reference is wrong"
    );
    assert_eq!(
        owner.controller,
        Some(true),
        "controller of owner-reference is wrong"
    );
    assert_eq!(owner.kind, project.kind, "kind of owner-reference is wrong");
    assert_eq!(
        owner.name,
        project.metadata.name.clone().unwrap(),
        "name of owner-reference is wrong"
    );
    assert_eq!(
        owner.uid,
        project.metadata.uid.clone().unwrap(),
        "uid of owner-reference is wrong: owner: {:#?}, project: {:#?}",
        owner,
        project
    );

    Ok(())
}
