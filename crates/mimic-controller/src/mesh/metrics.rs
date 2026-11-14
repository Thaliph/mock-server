//! `mimic_mesh_*` (spec §9).

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Registry, Unit};

pub type Labels = Vec<(String, String)>;

pub fn label(name: &str, value: &str) -> Labels {
    vec![(name.to_string(), value.to_string())]
}

#[derive(Clone)]
pub struct MeshMetrics {
    pub(crate) objects: Family<Labels, Gauge>,
    pub(crate) builds: Family<Labels, Counter>,
    pub(crate) build_duration: Histogram,
    pub(crate) watches: Gauge,
    pub(crate) truncated: Counter,
    pub(crate) cache_bytes: Gauge,
    pub(crate) gateway_api_present: Gauge,
    pub(crate) routes: Family<Labels, Gauge>,
    pub(crate) route_status: Family<Labels, Counter>,
}

impl Default for MeshMetrics {
    fn default() -> MeshMetrics {
        MeshMetrics {
            objects: Family::default(),
            builds: Family::default(),
            build_duration: Histogram::new([
                0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
            ]),
            watches: Gauge::default(),
            truncated: Counter::default(),
            cache_bytes: Gauge::default(),
            gateway_api_present: Gauge::default(),
            routes: Family::default(),
            route_status: Family::default(),
        }
    }
}

impl MeshMetrics {
    pub fn register(registry: &mut Registry) -> MeshMetrics {
        let metrics = MeshMetrics::default();
        registry.register(
            "mesh_objects",
            "Objects held by the mesh watches, by kind",
            metrics.objects.clone(),
        );
        registry.register(
            "mesh_view_builds",
            "Mesh views built, by result",
            metrics.builds.clone(),
        );
        registry.register_with_unit(
            "mesh_view_build_duration",
            "One mesh view build, canonical JSON and ETag included",
            Unit::Seconds,
            metrics.build_duration.clone(),
        );
        registry.register(
            "mesh_watches",
            "Mesh view requests held now",
            metrics.watches.clone(),
        );
        registry.register(
            "mesh_view_truncated",
            "Services left out of built views because of a bound",
            metrics.truncated.clone(),
        );
        registry.register_with_unit(
            "mesh_view_cache",
            "Bytes held by the mesh view cache",
            Unit::Bytes,
            metrics.cache_bytes.clone(),
        );
        registry.register(
            "mesh_gateway_api_present",
            "1 when the Gateway API HTTPRoute CRD is installed, else 0",
            metrics.gateway_api_present.clone(),
        );
        registry.register(
            "mesh_routes",
            "HTTPRoutes with a Service parent, by state (programmed or unsupported)",
            metrics.routes.clone(),
        );
        registry.register(
            "mesh_route_status",
            "status.parents patches of HTTPRoutes, by result (ok, conflict or error)",
            metrics.route_status.clone(),
        );
        metrics
    }
}
