use kube::{
    api::Resource,
    api::Reflector,
    client::APIClient,
};
use k8s_openapi::api::core::v1 as api;
use k8s_openapi::api::apps::v1 as apps;
use k8s_openapi::apimachinery::pkg::apis::meta::v1 as meta;

use crate::schematic::{
    configuration::Configuration,
    component::Component,
    traits::{
        Ingress,
        TraitBinding,
        TraitImplementation,
    },
    Status,
};

use std::collections::BTreeMap;

use serde_json::to_string_pretty as to_json;

/// Type alias for the results that all instantiation operations return
pub type InstigatorResult = Result<(), failure::Error>;

/// This error is returned when a component cannot be found.
#[derive(Fail, Debug)]
#[fail(display = "Component {} not found", component)]
pub struct ComponentNotFoundError {
    component: String,
}

const DEFAULT_NAMESPACE: &'static str = "default";

/// An Instigator takes an inbound object and manages the reconcilliation with the desired objects.
/// 
/// Any given Component may, underneath the hood, be composed of multiple Kubernetes objects.
/// For example, a ReplicableService will create (at least) a Deployment and a Service
/// (and probably a Secret or ConfigMap as well as some RBACs). The individual pieces are
/// managed by their respective WorkloadType. The Instigator's job is to read a component,
/// and then delegate to the correct WorkloadType.
/// 
/// Traits and Scopes are operational configuration. As such, it is not the responsibility of
/// the WorkloadType to manage those. Thus, after delegating work to the WorkloadType, the
/// Instigator will examine the Traits and Scopes requirements, and delegate those
/// processes to the appropriate Scope or TraitImpl.
/// 
/// (Terminological note: Hydra Traits are distinct from Rust traits. TraitImpl is the
/// Rust trait that represents a Hydra Trait)
/// 
/// Instigators know how to deal with the following operations:
/// - Add
/// - Modify
/// - Delete
#[derive(Clone)]
pub struct Instigator {
    client: APIClient,
    cache: Reflector<Component, Status>
}
impl Instigator {
    /// Create a new instigator
    /// 
    /// An instigator uses the reflector as a cache of Components, and will use the API client
    /// for creating and managing the component implementation.
    pub fn new(client: kube::client::APIClient, cache: Reflector<Component, Status>) -> Self {
        Instigator {
            client: client,
            cache: cache,
        }
    }
    /// Create new Kubernetes objects based on this config.
    pub fn add(&self, event: Resource<Configuration, Status>) -> InstigatorResult {
        let name = event.metadata.name.clone();
        // Find components
        let cache = self.cache.read().unwrap();
        let comp_def = cache.get(event.spec.component.as_str()).ok_or(ComponentNotFoundError{component: event.spec.component})?;

        // Resolve parameters
        // Instantiate components
        let workload = self.load_workload_type(name.clone(), comp_def)?;
        workload.add()?;
        // Attach traits
        // FIXME: This is currently not working because workload.add is returning an error having to do with the
        // formatting of the response object. :angry-eyes:
        for t in event.spec.traits.unwrap_or(vec![]).iter() {
            println!("Searching for trait {}", t.name.as_str());
            let imp = self.load_trait(name.clone(), t)?;
            imp.add(DEFAULT_NAMESPACE.into(), self.client.clone())?;
        }
        Ok(())
    }
    /// Modify existing Kubernetes objects based on config and workload type.
    pub fn modify(&self, event: Resource<Configuration, Status>) -> InstigatorResult {
        // Find components
        let cache = self.cache.read().unwrap();
        let comp_def = cache.get(event.spec.component.as_str()).ok_or(ComponentNotFoundError{component: event.spec.component})?;

        // Resolve parameters
        // Instantiate components
        let workload = self.load_workload_type(event.metadata.name, comp_def)?;
        workload.modify()
    }
    /// Delete the Kubernetes objects associated with this config.
    pub fn delete(&self, event: Resource<Configuration, Status>) -> InstigatorResult {
        let name = event.metadata.name.clone();
        // Find component
        let cache = self.cache.read().unwrap();
        let cname = event.spec.component.clone();
        let comp_def = cache.get(cname.as_str()).ok_or(ComponentNotFoundError{component: cname})?;
        
        // Delete traits
        // Right now, a failed deletion on a trait is just logged, and is not
        // a fatail error.
        for t in event.spec.traits.unwrap_or(vec![]).iter() {
            println!("Deleting trait {}", t.name.as_str());
            let imp = self.load_trait(name.clone(), t)?;
            let res = imp.delete(DEFAULT_NAMESPACE.into(), self.client.clone());
            if res.is_err() {
                println!("Error deleting trait for {}: {}", t.name.as_str(), res.unwrap_err());
            }
        }

        // Delete component
        self.load_workload_type(event.metadata.name, comp_def)?.delete()
    }

    fn load_workload_type(&self, name: String, comp: &Resource<Component, Status>) -> Result<CoreWorkloadType, failure::Error> {
        println!("Looking up {}", name);
        match comp.spec.workload_type.as_str() {
            "core.hydra.io/v1alpha1.ReplicatedService" => {
                let rs = ReplicatedService{
                    name: name, 
                    namespace: DEFAULT_NAMESPACE.into(),
                    definition: comp.spec.clone(),
                    client: self.client.clone(),
                };
                Ok(CoreWorkloadType::ReplicatedServiceType(rs))
            },
            "core.hydra.io/v1alpha1.Singleton" => {
                let sing = Singleton::new(name, DEFAULT_NAMESPACE.into(), comp.spec.clone(), self.client.clone());
                Ok(CoreWorkloadType::SingletonType(sing))
            },
            //"core.hydra.io/v1alpha1.Task" => {},
            //"core.hydra.io/v1alpha1.ReplicableTask" => {},
            _ => {
                Err(format_err!("workloadType {} is unknown", comp.spec.workload_type))
            }
        }
    }

    fn load_trait(&self, name: String, binding: &TraitBinding) -> Result<impl TraitImplementation, failure::Error> {
        match binding.name.as_str() {
            "ingress" => {
                Ok(Ingress::new(80, name, None, None))
            }
            _ => Err(format_err!("unknown trait {}", binding.name))
        }
    }
}
/// WorkloadType describes one of the available workload types.
/// 
/// An implementation of a workload type must be able to add, modify, and delete itself.
pub trait WorkloadType {
    fn add(&self)->InstigatorResult;
    fn modify(&self)->InstigatorResult;
    fn delete(&self)->InstigatorResult;
}

enum CoreWorkloadType {
    SingletonType(Singleton),
    ReplicatedServiceType(ReplicatedService),
}

impl CoreWorkloadType {
    fn delete(&self) -> InstigatorResult {
        match self {
            CoreWorkloadType::SingletonType(sing) => sing.delete(),
            CoreWorkloadType::ReplicatedServiceType(repl) => repl.delete(),
        }
    }
    fn add(&self) -> InstigatorResult {
        match self {
            CoreWorkloadType::SingletonType(sing) => sing.add(),
            CoreWorkloadType::ReplicatedServiceType(repl) => repl.add(),
        }
    }
    fn modify(&self) -> InstigatorResult {
        Err(format_err!("modify operation is not implemented"))
    }
}

/// Singleton represents the Singleton Workload Type, as defined in the Hydra specification.
/// 
/// It is currently implemented as a Kubernetes Pod with a Service in front of it.
struct Singleton {
    name: String,
    namespace: String,
    definition: Component,
    client: APIClient,
}
impl Singleton {
    /// Create a new Singleton destined for a particular namespace
    fn new(name: String, namespace: String, definition: Component, client: APIClient) -> Self {
        Singleton {
            name: name,
            namespace: namespace,
            definition: definition,
            client: client,
        }
    }
    /// Create a Pod definition that describes this Singleton
    fn to_pod(&self) -> api::Pod {
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), self.name.clone());
        api::Pod{
            // TODO: Could make this generic.
            metadata: Some(meta::ObjectMeta{
                name: Some(self.name.clone()),
                labels: Some(labels),
                ..Default::default()
            }),
            spec: Some(self.definition.to_pod_spec()),
            ..Default::default()
        }
    }
    /// Create a service if this component has a port.
    fn to_service(&self) -> Option<api::Service> {
        self.definition.listening_port().and_then(|port| {
            let mut labels = BTreeMap::new();
            labels.insert("app".to_string(), self.name.clone());
            Some(api::Service{
                metadata: Some(meta::ObjectMeta{
                    name: Some(self.name.clone()),
                    labels: Some(labels.clone()),
                    ..Default::default()
                }),
                spec: Some(api::ServiceSpec{
                    selector: Some(labels),
                    ports: Some(vec![port.to_service_port()]),
                    ..Default::default()
                }),
                ..Default::default()
            })
        })
        
    }
}
impl WorkloadType for Singleton {
    fn add(&self) -> InstigatorResult {
        let pod = self.to_pod();
        let (req, _) = api::Pod::create_namespaced_pod(self.namespace.as_str(), &pod, Default::default())?;

        // We force the decoded value into a serde_json::Value because we don't care if Kubernetes returns a
        // malformed body. We just want the response code validated by APIClient.
        let res: Result<serde_json::Value, failure::Error> = self.client.request(req);
        if res.is_err() {
            return Err(res.unwrap_err())
        }

        /*
        if res.is_err() {
            // FIXME: We seem to be getting a formatting error for the response, but I don't know what is causing it
            println!("Pod: {}", to_json(&pod).unwrap());
            println!("Warning: {:?}", res);
        }
        */

        match self.to_service() {
            Some(svc) => {
                println!("Service:\n{}", to_json(&svc).unwrap());
                let (sreq, _) = api::Service::create_namespaced_service(self.namespace.as_str(), &svc, Default::default())?;
                let sres: Result<serde_json::Value, failure::Error> = self.client.request(sreq);
                sres.and_then(|_o| Ok(()))
            }
            // No service to create
            None => {
                println!("Not attaching service to pod with no container ports.");
                Ok(())
            }
        }
    }
    fn modify(&self) -> InstigatorResult {
        Err(format_err!("Not implemented"))
    }
    fn delete(&self) -> InstigatorResult {
        let (req, _) = api::Pod::delete_namespaced_pod(self.name.as_str(), self.namespace.as_str(), Default::default())?;

        // By decoding into serde_json::Value, we are bypassing all checks on the return data, which 
        // is fine. We don't actually need any of it, and the APIClient checks for status and error
        // on our behalf.
        let pres: Result<serde_json::Value, failure::Error> = self.client.request(req);

        match self.to_service() {
            Some(_) => {
                let (sreq, _) = api::Service::delete_namespaced_service(self.name.as_str(), self.namespace.as_str(), Default::default())?;
                let sres: Result<serde_json::Value, failure::Error> = self.client.request(sreq);
                // If either op fails, return the error. Otherwise, just return Ok(()).
                pres.and(sres).and_then(|_o| Ok(()))
            }
            None => {
                pres.and_then(|_| Ok(()))
            }
        }
    }
}


/// A Replicated Service can take one component and scale it up or down.
struct ReplicatedService {
    name: String,
    namespace: String,
    definition: Component,
    client: APIClient,
}

impl ReplicatedService {
    /// Create a Pod definition that describes this Singleton
    fn to_deployment(&self) -> apps::Deployment {
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), self.name.clone());
        apps::Deployment{
            // TODO: Could make this generic.
            metadata: Some(meta::ObjectMeta{
                name: Some(self.name.clone()),
                labels: Some(labels),
                ..Default::default()
            }),
            spec: Some(self.definition.to_deployment_spec(self.name.clone())),
            ..Default::default()
        }
    }
    /// Create a service if this component has a port.
    fn to_service(&self) -> Option<api::Service> {
        self.definition.listening_port().and_then(|port| {
            let mut labels = BTreeMap::new();
            labels.insert("app".to_string(), self.name.clone());
            Some(api::Service{
                metadata: Some(meta::ObjectMeta{
                    name: Some(self.name.clone()),
                    labels: Some(labels.clone()),
                    ..Default::default()
                }),
                spec: Some(api::ServiceSpec{
                    selector: Some(labels),
                    ports: Some(vec![port.to_service_port()]),
                    ..Default::default()
                }),
                ..Default::default()
            })
        })
        
    }
}

impl WorkloadType for ReplicatedService {
    fn add(&self) -> InstigatorResult {
        let deployment = self.to_deployment();
        let (req, _) = apps::Deployment::create_namespaced_deployment(self.namespace.as_str(), &deployment, Default::default())?;

        // We force the decoded value into a serde_json::Value because we don't care if Kubernetes returns a
        // malformed body. We just want the response code validated by APIClient.
        let res: Result<serde_json::Value, failure::Error> = self.client.request(req);
        if res.is_err() {
            return Err(res.unwrap_err())
        }
        match self.to_service() {
            Some(svc) => {
                println!("Service:\n{}", to_json(&svc).unwrap());
                let (sreq, _) = api::Service::create_namespaced_service(self.namespace.as_str(), &svc, Default::default())?;
                let sres: Result<serde_json::Value, failure::Error> = self.client.request(sreq);
                res.and(sres).and_then(|_o| Ok(()))
            }
            // No service to create
            None => {
                println!("Not attaching service to pod with no container ports.");
                Ok(())
            }
        }
    }
    fn modify(&self) -> InstigatorResult {
        Err(format_err!("Not implemented"))
    }
    fn delete(&self) -> InstigatorResult {
        
        let (req, _) = apps::Deployment::delete_namespaced_deployment(self.name.as_str(), self.namespace.as_str(), Default::default())?;

        let dres: Result<serde_json::Value, failure::Error> = self.client.request(req);

        match self.to_service() {
            Some(_) => {
                let (sreq, _) = api::Service::delete_namespaced_service(self.name.as_str(), self.namespace.as_str(), Default::default())?;
                let sres: Result<serde_json::Value, failure::Error> = self.client.request(sreq);
                sres.and_then(|_| Ok(()))
            }
            None => dres.and_then(|_| Ok(()))
        }
    }
}
