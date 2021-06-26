use futures::{StreamExt, TryStreamExt};
use k8s::api::{admissionregistration::v1::MutatingWebhookConfiguration, core::v1::Service};
use k8s_openapi as k8s;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use krator::OperatorRuntime;
use kube::api::{self, DeleteParams};
use kube::api::{PostParams, WatchEvent};
use kube::config;
use noqnoqnoq::{
    project::{Project, ProjectOperator},
    self_service::{project, Sample},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{convert::TryFrom, path::Path};
use tokio::select;
use tokio::task::JoinHandle;
use tokio::time;

pub async fn before_each() -> anyhow::Result<(kube::Client, ProjectOperator)> {
    let mut kubeconfig = config::Kubeconfig::read_from(Path::new("./kind.kubeconfig"))?;
    // use hostname instead of ip: https://github.com/ctz/rustls/issues/206
    kubeconfig.clusters[0].cluster.server = kubeconfig.clusters[0]
        .cluster
        .server
        .replace("127.0.0.1", "localhost");
    let mut config =
        config::Config::from_custom_kubeconfig(kubeconfig, &config::KubeConfigOptions::default())
            .await?;

    config.timeout = Some(Duration::from_secs(10));

    let client = kube::Client::try_from(config.clone())?;

    // basic check so we fail early if k8s communication does not work
    assert!(
        client.apiserver_version().await.is_ok(),
        "communication with kubernetes should work"
    );

    // there is probably a better way FnOnce?
    let _ = reinstall_self_service_crd(&client).await?;

    let operator = project::ProjectOperator::new(client.clone(), "admin", "default")
        .await
        .unwrap();
    let mut runtime = OperatorRuntime::new(&config, operator.clone(), None);

    tokio::spawn(async move { runtime.start().await });

    Ok((client, operator))
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

    let wait_for_crd_created = wait_for_state(&api, &name, WaitForState::Created);
    let crd = noqnoqnoq::helper::install_crd(&client, &project::Project::crd()).await?;
    let _ = wait_for_crd_created.await?;

    const NAMESPACE: &'static str = "default";
    let (service, secret, config) = Project::admission_webhook_resources(NAMESPACE);

    {
        let api = kube::Api::<Service>::namespaced(client.clone(), NAMESPACE);
        let service_name = service.metadata.name.as_ref().unwrap();
        if api.get(service_name).await.is_ok() {
            wait_for_state(&api, service_name, WaitForState::Deleted).await?;
        }
    }

    krator::admission::WebhookResources(service, secret, config.clone())
        .apply_owned(&client.clone(), &crd)
        .await?;

    // delete the webhook config again as we will not run within the cluster during testing
    {
        let api = kube::Api::<MutatingWebhookConfiguration>::all(client.clone());
        let config_name = config.metadata.name.as_ref().unwrap();
        let config_deletion = wait_for_state(&api, config_name, WaitForState::Deleted);
        api.delete(config_name, &DeleteParams::default()).await?;
        config_deletion.await?
    }

    Ok(())
}

pub fn wait_for_state<K: 'static>(
    api: &kube::Api<K>,
    name: &String,
    state: WaitForState,
) -> JoinHandle<()>
where
    K: std::fmt::Debug
        + k8s_openapi::Resource
        + kube::Resource
        + Clone
        + std::marker::Send
        + for<'de> serde::de::Deserialize<'de>,
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

        let resource_version;
        loop {
            match api.list(&lp).await {
                Ok(list) => {
                    resource_version = list.metadata.resource_version.unwrap();
                    break;
                }
                _ => {
                    tokio::time::sleep(time::Duration::from_millis(100)).await;
                }
            }
        }

        // check whether state is already reached before starting a watch
        let get_res = api.get(&name).await;
        match state {
            WaitForState::Created if get_res.is_ok() => return (),
            WaitForState::Deleted if get_res.is_err() => return (),
            _ => {}
        }

        let mut stream = api.watch(lp, &resource_version).await.unwrap().boxed();

        let print_info = {
            |e: &WatchEvent<K>, resource: &K| {
                println!(
                    "  - {:?} for {} with name {} received",
                    e,
                    K::KIND,
                    (resource.meta().clone() as ObjectMeta).name.unwrap(),
                );
            }
        };

        loop {
            match stream.try_next().await {
                Ok(Some(status)) => match status.clone() {
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
                },
                Ok(None) => {
                    // happens, if nothing watchable was found (e.g. watching for somehing in a namespace
                    // that does not exist yet
                    println!("  - too early to watch {} with name {}", K::KIND, name,);
                    tokio::time::sleep(time::Duration::from_millis(250)).await;
                    break;
                }
                Err(e) => {
                    println!(
                        " - ERROR getting {} with name {} from stream: {}",
                        K::KIND,
                        name,
                        e
                    );
                }
            }
        }
        // again: Kubernetes-API does not seem to be strictly consistent ...
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    })
}

pub async fn install_project(
    client: &kube::Client,
    name: &String,
) -> anyhow::Result<project::Project> {
    let timeout_secs = 10;
    let project_api: kube::Api<project::Project> = kube::Api::all(client.clone());

    let wait_for_namespace_created_handle = wait_for_state(
        &kube::Api::<Namespace>::all(client.clone()),
        name,
        WaitForState::Created,
    );

    let wait_for_project_created_handle =
        wait_for_state(&project_api, &name, WaitForState::Created);

    let project = project::Project::new(name.as_str(), project::ProjectSpec::sample());
    let project_resource = project_api.create(&PostParams::default(), &project).await;

    assert!(
        project_resource.is_ok(),
        "creating a new self service project should work correclty: {}",
        project_resource.err().unwrap()
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

    Ok(project_resource.unwrap())
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
    R: kube::Resource + k8s_openapi::Resource,
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
