// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

use std::time::Instant;

use drain::Watch;

use hyper::header::FORWARDED;

use tokio::net::{TcpListener, TcpStream};

use tracing::{debug, error, info, info_span, trace_span, warn, Instrument};

use crate::config::ProxyMode;
use crate::identity::Identity;

use crate::proxy::metrics::Reporter;
use crate::proxy::{metrics, pool, ConnectionOpen, ConnectionResult};
use crate::proxy::{util, Error, ProxyInputs, TraceParent, BAGGAGE_HEADER, TRACEPARENT_HEADER};

use crate::proxy::h2_client::H2Stream;
use crate::state::service::ServiceDescription;
use crate::state::workload::gatewayaddress::Destination;
use crate::state::workload::{address::Address, NetworkAddress, Protocol, Workload};
use crate::strng::Strng;
use crate::{assertions, copy, proxy, socket, strng};

pub struct Outbound {
    pi: ProxyInputs,
    drain: Watch,
    listener: TcpListener,
}

impl Outbound {
    pub(super) async fn new(mut pi: ProxyInputs, drain: Watch) -> Result<Outbound, Error> {
        let listener: TcpListener = pi
            .socket_factory
            .tcp_bind(pi.cfg.outbound_addr)
            .map_err(|e| Error::Bind(pi.cfg.outbound_addr, e))?;
        let transparent = super::maybe_set_transparent(&pi, &listener)?;
        // Override with our explicitly configured setting
        if pi.cfg.enable_original_source.is_none() {
            let mut cfg = (*pi.cfg).clone();
            cfg.enable_original_source = Some(transparent);
            pi.cfg = Arc::new(cfg);
        }

        info!(
            address=%listener.local_addr().expect("local_addr available"),
            component="outbound",
            transparent,
            "listener established",
        );
        Ok(Outbound {
            pi,
            listener,
            drain,
        })
    }

    pub(super) fn address(&self) -> SocketAddr {
        self.listener.local_addr().expect("local_addr available")
    }

    pub(super) async fn run(self) {
        // Since we are spawning autonomous tasks to handle outbound connections for a single workload,
        // we can have situations where the workload is deleted, but a task is still "stuck"
        // waiting for a server response stream on a HTTP/2 connection or whatnot.
        //
        // So use a drain to nuke tasks that might be stuck sending.
        let (sub_drain_signal, sub_drain) = drain::channel();
        let pi = Arc::new(self.pi);

        let pool = proxy::pool::WorkloadHBONEPool::new(
            pi.cfg.clone(),
            pi.socket_factory.clone(),
            pi.cert_manager.clone(),
        );
        let accept = async move {
            loop {
                // Asynchronously wait for an inbound socket.
                let socket = self.listener.accept().await;
                let start_outbound_instant = Instant::now();
                let outbound_drain = sub_drain.clone();
                match socket {
                    Ok((stream, _remote)) => {
                        let mut oc = OutboundConnection {
                            pi: pi.clone(),
                            id: TraceParent::new(),
                            pool: pool.clone(),
                        };
                        let span = info_span!("outbound", id=%oc.id);
                        let serve_outbound_connection = (async move {
                            debug!(dur=?start_outbound_instant.elapsed(), id=%oc.id, "outbound spawn START");
                            // Since this task is spawned, make sure we are guaranteed to terminate
                            tokio::select! {
                                _ = outbound_drain.signaled() => {
                                    debug!("outbound drain signaled");
                                }
                                _ = oc.proxy(stream) => {}
                            }
                            debug!(dur=?start_outbound_instant.elapsed(), id=%oc.id, "outbound spawn DONE");
                        }).instrument(span);

                        assertions::size_between_ref(1000, 1750, &serve_outbound_connection);
                        tokio::spawn(serve_outbound_connection);
                    }
                    Err(e) => {
                        if util::is_runtime_shutdown(&e) {
                            return;
                        }
                        error!("Failed TCP handshake {}", e);
                    }
                }
            }
        }
        .in_current_span();

        // Stop accepting once we drain.
        // Note: we are *not* waiting for all connections to be closed. In the future, we may consider
        // this, but will need some timeout period, as we have no back-pressure mechanism on connections.
        tokio::select! {
            res = accept => { res }
            _ = self.drain.signaled() => {
                debug!("outbound drained, dropping any outbound connections");
                sub_drain_signal.drain().await;
                info!("outbound drained");
            }
        }
    }
}

pub(super) struct OutboundConnection {
    pub(super) pi: Arc<ProxyInputs>,
    pub(super) id: TraceParent,
    pub(super) pool: proxy::pool::WorkloadHBONEPool,
}

impl OutboundConnection {
    async fn proxy(&mut self, source_stream: TcpStream) {
        let source_addr =
            socket::to_canonical(source_stream.peer_addr().expect("must receive peer addr"));
        let dst_addr = socket::orig_dst_addr_or_default(&source_stream);
        self.proxy_to(source_stream, source_addr, dst_addr, false)
            .await;
    }

    // this is a cancellable outbound proxy. If `out_drain` is a Watch drain, will resolve
    // when the drain is signaled, or the outbound stream is completed, no matter what.
    //
    // If `out_drain` is none, will only resolve when the outbound stream is terminated.
    //
    // If using `proxy_to` in `tokio::spawn` tasks, it is recommended to use a drain, to guarantee termination
    // and prevent "zombie" outbound tasks.
    pub async fn proxy_to_cancellable(
        &mut self,
        stream: TcpStream,
        remote_addr: SocketAddr,
        orig_dst_addr: SocketAddr,
        block_passthrough: bool,
        out_drain: Option<Watch>,
    ) {
        match out_drain {
            Some(drain) => {
                tokio::select! {
                        _ = drain.signaled() => {
                            info!("drain signaled");
                        }
                        res = self.proxy_to(stream, remote_addr, orig_dst_addr, block_passthrough) => res
                }
            }
            None => {
                self.proxy_to(stream, remote_addr, orig_dst_addr, block_passthrough)
                    .await;
            }
        }
    }

    async fn proxy_to(
        &mut self,
        mut source_stream: TcpStream,
        source_addr: SocketAddr,
        dest_addr: SocketAddr,
        block_passthrough: bool,
    ) {
        let start = Instant::now();

        // Block calls to ztunnel directly, unless we are in "in-pod".
        // For in-pod, this isn't an issue and is useful: this allows things like prometheus scraping ztunnel.
        if self.pi.cfg.proxy_mode == ProxyMode::Shared
            && Some(dest_addr.ip()) == self.pi.cfg.local_ip
            && !self.pi.cfg.inpod_enabled
        {
            metrics::log_early_deny(source_addr, dest_addr, Reporter::source, Error::SelfCall);
            return;
        }
        let req = match Box::pin(self.build_request(source_addr.ip(), dest_addr)).await {
            Ok(req) => req,
            Err(err) => {
                metrics::log_early_deny(source_addr, dest_addr, Reporter::source, err);
                return;
            }
        };
        debug!(
            "request from {} to {} via {} type {:#?}",
            req.source.name, dest_addr, req.gateway, req.request_type
        );
        if block_passthrough && req.destination_workload.is_none() {
            // This is mostly used by socks5. For typical outbound calls, we need to allow calls to arbitrary
            // domains. But for socks5
            metrics::log_early_deny(
                source_addr,
                dest_addr,
                Reporter::source,
                Error::UnknownDestination(req.destination.ip()),
            );
            return;
        }
        // TODO: should we use the original address or the actual address? Both seems nice!
        let _conn_guard =
            self.pi
                .connection_manager
                .track_outbound(source_addr, dest_addr, req.gateway);

        let metrics = self.pi.metrics.clone();
        let hbone_target = if req.protocol == Protocol::HBONE {
            Some(req.destination)
        } else {
            None
        };
        let result_tracker = Box::new(ConnectionResult::new(
            source_addr,
            req.gateway,
            hbone_target,
            start,
            Self::conn_metrics_from_request(&req),
            metrics,
        ));

        let res = match req.protocol {
            Protocol::HBONE => {
                self.proxy_to_hbone(source_stream, source_addr, &req, &result_tracker)
                    .await
            }
            Protocol::TCP => {
                self.proxy_to_tcp(&mut source_stream, &req, &result_tracker)
                    .await
            }
        };
        result_tracker.record(res)
    }

    async fn proxy_to_hbone(
        &mut self,
        stream: TcpStream,
        remote_addr: SocketAddr,
        req: &Request,
        connection_stats: &ConnectionResult,
    ) -> Result<(), Error> {
        debug!(
            "proxy to {} using HBONE via {} type {:#?}",
            req.destination, req.gateway, req.request_type
        );

        let upgraded = Box::pin(self.build_hbone_request(remote_addr, &req)).await?;

        copy::copy_bidirectional(stream, upgraded, connection_stats).await
    }

    async fn build_hbone_request(
        &mut self,
        remote_addr: SocketAddr,
        req: &&Request,
    ) -> Result<H2Stream, Error> {
        let mut allowed_sans: Vec<Identity> = Vec::new();
        for san in req.upstream_sans.iter() {
            match Identity::from_str(san) {
                Ok(ident) => allowed_sans.push(ident.clone()),
                Err(err) => {
                    warn!("error parsing SAN {}: {}", san, err)
                }
            }
        }

        allowed_sans.push(
            req.expected_identity
                .clone()
                .expect("HBONE request must have expected identity"),
        );
        let dst_identity = allowed_sans;

        let pool_key = Box::new(pool::WorkloadKey {
            src_id: req.source.identity(),
            dst_id: dst_identity.clone(),
            src: remote_addr.ip(),
            dst: req.gateway,
        });

        let mut f = http_types::proxies::Forwarded::new();
        f.add_for(remote_addr.to_string());
        if let Some(svc) = &req.destination_service {
            f.set_host(svc.hostname.as_str());
        }

        let request = http::Request::builder()
            .uri(&req.destination.to_string())
            .method(hyper::Method::CONNECT)
            .version(hyper::Version::HTTP_2)
            .header(BAGGAGE_HEADER, baggage(req, self.pi.cfg.cluster_id.clone()))
            .header(FORWARDED, f.value().expect("Forwarded value is infallible"))
            .header(TRACEPARENT_HEADER, self.id.header())
            .body(())
            .expect("builder with known status code should not fail");

        let upgraded = Box::pin(self.pool.send_request_pooled(&pool_key, request))
            .instrument(trace_span!("outbound connect"))
            .await?;
        Ok(upgraded)
    }

    async fn proxy_to_tcp(
        &mut self,
        stream: &mut TcpStream,
        req: &Request,
        connection_stats: &ConnectionResult,
    ) -> Result<(), Error> {
        debug!(
            "Proxying to {} using TCP via {} type {:?}",
            req.destination, req.gateway, req.request_type
        );
        // Create a TCP connection to upstream
        let local = if self.pi.cfg.enable_original_source.unwrap_or_default() {
            super::get_original_src_from_stream(stream)
        } else {
            None
        };
        let mut outbound =
            super::freebind_connect(local, req.gateway, self.pi.socket_factory.as_ref()).await?;

        // Proxying data between downstream and upstream
        copy::copy_bidirectional(stream, &mut outbound, connection_stats).await
    }

    fn conn_metrics_from_request(req: &Request) -> ConnectionOpen {
        ConnectionOpen {
            reporter: Reporter::source,
            derived_source: None,
            source: Some(req.source.clone()),
            destination: req.destination_workload.clone(),
            connection_security_policy: if req.protocol == Protocol::HBONE {
                metrics::SecurityPolicy::mutual_tls
            } else {
                metrics::SecurityPolicy::unknown
            },
            destination_service: req.destination_service.clone(),
        }
    }

    async fn build_request(
        &self,
        downstream: IpAddr,
        target: SocketAddr,
    ) -> Result<Box<Request>, Error> {
        let downstream_network_addr = NetworkAddress {
            network: strng::new(&self.pi.cfg.network),
            address: downstream,
        };
        let source_workload = match self.pi.state.fetch_workload(&downstream_network_addr).await {
            Some(wl) => wl,
            None => return Err(Error::UnknownSource(downstream)),
        };
        if let Some(ref wl_info) = self.pi.proxy_workload_info {
            // make sure that the workload we fetched matches the workload info we got over ZDS.
            if !wl_info.matches(&source_workload) {
                return Err(Error::MismatchedSource(downstream, wl_info.clone()));
            }
        }

        // If this is to-service traffic check for a service waypoint
        // Capture result of whether or not this is svc addressed
        let svc_addressed = if let Some(Address::Service(s)) = self
            .pi
            .state
            .fetch_destination(&Destination::Address(NetworkAddress {
                network: strng::new(&self.pi.cfg.network),
                address: target.ip(),
            }))
            .await
        {
            // if we have a waypoint for this svc, use it; otherwise route traffic normally
            if let Some(wp) = s.waypoint.clone() {
                let waypoint_vip = match wp.destination {
                    Destination::Address(a) => a.address,
                    Destination::Hostname(_) => {
                        return Err(proxy::Error::UnknownWaypoint(
                            "hostname lookup not supported yet".to_string(),
                        ));
                    }
                };
                let waypoint_vip = SocketAddr::new(waypoint_vip, wp.hbone_mtls_port);
                let waypoint_us = self
                    .pi
                    .state
                    .fetch_upstream(self.pi.cfg.network.clone(), &source_workload, waypoint_vip)
                    .await
                    .ok_or(proxy::Error::UnknownWaypoint(
                        "unable to determine waypoint upstream".to_string(),
                    ))?;

                let waypoint_workload = waypoint_us.workload;
                let waypoint_ip = self
                    .pi
                    .state
                    .pick_workload_destination(
                        &waypoint_workload,
                        &source_workload,
                        self.pi.metrics.clone(),
                    )
                    .await?; // if we can't load balance just return the error

                let waypoint_socket_address = SocketAddr::new(waypoint_ip, waypoint_us.port);
                let id = waypoint_workload.identity();
                return Ok(Box::new(Request {
                    protocol: Protocol::HBONE,
                    source: source_workload,
                    destination: target,
                    destination_workload: Some(waypoint_workload),
                    destination_service: Some(ServiceDescription::from(&*s)),
                    expected_identity: Some(id),
                    gateway: waypoint_socket_address,
                    request_type: RequestType::ToServerWaypoint,
                    upstream_sans: waypoint_us.sans,
                }));
            }
            // this was service addressed but we did not find a waypoint
            true
        } else {
            // this wasn't service addressed
            false
        };

        // TODO: we want a single lock for source and upstream probably...?
        let us = match self
            .pi
            .state
            .fetch_upstream(source_workload.network.clone(), &source_workload, target)
            .await
        {
            Some(us) => us,
            None => {
                // For case no upstream found, passthrough it
                return Ok(Box::new(Request {
                    protocol: Protocol::TCP,
                    source: source_workload,
                    destination: target,
                    destination_workload: None,
                    destination_service: None,
                    expected_identity: None,
                    gateway: target,
                    request_type: RequestType::Passthrough,
                    upstream_sans: vec![],
                }));
            }
        };

        let workload_ip = self
            .pi
            .state
            .pick_workload_destination(&us.workload, &source_workload, self.pi.metrics.clone())
            .await?;

        let from_waypoint = proxy::check_from_waypoint(
            &self.pi.state,
            &us.workload,
            Some(&source_workload.identity()),
            &downstream_network_addr.address,
        )
        .await;

        // Don't traverse waypoint twice if the source is sandwich-outbound.
        // Don't traverse waypoint if traffic was addressed to a service which did not have a waypoint
        if !from_waypoint && !svc_addressed {
            // For case upstream server has enabled waypoint
            match self
                .pi
                .state
                .fetch_waypoint(&us.workload, &source_workload, workload_ip)
                .await
            {
                Ok(None) => {} // workload doesn't have a waypoint; this is fine
                Ok(Some(waypoint_us)) => {
                    let waypoint_workload = waypoint_us.workload;
                    let waypoint_ip = self
                        .pi
                        .state
                        .pick_workload_destination(
                            &waypoint_workload,
                            &source_workload,
                            self.pi.metrics.clone(),
                        )
                        .await?;
                    let waypoint_socket_address = SocketAddr::new(waypoint_ip, waypoint_us.port);
                    let id = waypoint_workload.identity();
                    return Ok(Box::new(Request {
                        // Always use HBONE here
                        protocol: Protocol::HBONE,
                        source: source_workload,
                        // Use the original VIP, not translated
                        destination: target,
                        destination_workload: Some(waypoint_workload),
                        destination_service: us.destination_service.clone(),
                        expected_identity: Some(id),
                        gateway: waypoint_socket_address,
                        request_type: RequestType::ToServerWaypoint,
                        upstream_sans: us.sans,
                    }));
                }
                // we expected the workload to have a waypoint, but could not find one
                Err(e) => return Err(Error::UnknownWaypoint(e.to_string())),
            }
        }

        // only change the port if we're sending HBONE
        let gw_addr = match us.workload.protocol {
            Protocol::HBONE => SocketAddr::from((workload_ip, self.pi.hbone_port)),
            Protocol::TCP => SocketAddr::from((workload_ip, us.port)),
        };

        // For case no waypoint for both side and direct to remote node proxy
        Ok(Box::new(Request {
            protocol: us.workload.protocol,
            source: source_workload,
            destination: SocketAddr::from((workload_ip, us.port)),
            destination_workload: Some(us.workload.clone()),
            destination_service: us.destination_service.clone(),
            expected_identity: Some(us.workload.identity()),
            gateway: gw_addr,
            request_type: RequestType::Direct,
            upstream_sans: us.sans,
        }))
    }
}

fn baggage(r: &Request, cluster: String) -> String {
    format!("k8s.cluster.name={cluster},k8s.namespace.name={namespace},k8s.{workload_type}.name={workload_name},service.name={name},service.version={version}",
            namespace = r.source.namespace,
            workload_type = r.source.workload_type,
            workload_name = r.source.workload_name,
            name = r.source.canonical_name,
            version = r.source.canonical_revision,
    )
}

#[derive(Debug)]
struct Request {
    protocol: Protocol,
    source: Workload,
    destination: SocketAddr,
    // The intended destination workload. This is always the original intended target, even in the case
    // of other proxies along the path.
    destination_workload: Option<Workload>,
    destination_service: Option<ServiceDescription>,
    // The identity we will assert for the next hop; this may not be the same as destination_workload
    // in the case of proxies along the path.
    expected_identity: Option<Identity>,
    gateway: SocketAddr,
    request_type: RequestType,

    upstream_sans: Vec<Strng>,
}

#[derive(PartialEq, Debug)]
enum RequestType {
    /// ToServerWaypoint refers to requests targeting a server waypoint proxy
    ToServerWaypoint,
    /// Direct requests are made directly to a intended backend pod
    Direct,
    /// Passthrough refers to requests with an unknown target
    Passthrough,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;

    use super::*;
    use crate::config::Config;
    use crate::proxy::connection_manager::ConnectionManager;
    use crate::test_helpers::helpers::test_proxy_metrics;
    use crate::test_helpers::new_proxy_state;
    use crate::xds::istio::workload::address::Type as XdsAddressType;
    use crate::xds::istio::workload::NetworkAddress as XdsNetworkAddress;
    use crate::xds::istio::workload::Port;
    use crate::xds::istio::workload::Service as XdsService;
    use crate::xds::istio::workload::TunnelProtocol as XdsProtocol;
    use crate::xds::istio::workload::Workload as XdsWorkload;
    use crate::{identity, xds};

    async fn run_build_request(
        from: &str,
        to: &str,
        xds: XdsAddressType,
        expect: Option<ExpectedRequest<'_>>,
    ) {
        let cfg = Arc::new(Config {
            local_node: Some("local-node".to_string()),
            ..crate::config::parse_config().unwrap()
        });
        let source = XdsWorkload {
            uid: "cluster1//v1/Pod/ns/source-workload".to_string(),
            name: "source-workload".to_string(),
            namespace: "ns".to_string(),
            addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 1])],
            node: "local-node".to_string(),
            ..Default::default()
        };
        let waypoint = XdsWorkload {
            uid: "cluster1//v1/Pod/ns/waypoint-workload".to_string(),
            name: "waypoint-workload".to_string(),
            namespace: "ns".to_string(),
            addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 10])],
            node: "local-node".to_string(),
            service_account: "waypoint-sa".to_string(),
            ..Default::default()
        };
        let state = match xds {
            XdsAddressType::Workload(wl) => new_proxy_state(&[source, waypoint, wl], &[], &[]),
            XdsAddressType::Service(svc) => new_proxy_state(&[source, waypoint], &[svc], &[]),
        };

        let sock_fact = std::sync::Arc::new(crate::proxy::DefaultSocketFactory);
        let cert_mgr = identity::mock::new_secret_manager(Duration::from_secs(10));
        let outbound = OutboundConnection {
            pi: Arc::new(ProxyInputs {
                cert_manager: identity::mock::new_secret_manager(Duration::from_secs(10)),
                state,
                hbone_port: 15008,
                cfg: cfg.clone(),
                metrics: test_proxy_metrics(),
                socket_factory: sock_fact.clone(),
                proxy_workload_info: None,
                connection_manager: ConnectionManager::default(),
            }),
            id: TraceParent::new(),
            pool: pool::WorkloadHBONEPool::new(cfg, sock_fact, cert_mgr.clone()),
        };

        let req = outbound
            .build_request(from.parse().unwrap(), to.parse().unwrap())
            .await
            .ok();
        if let Some(r) = req {
            assert_eq!(
                expect,
                Some(ExpectedRequest {
                    protocol: r.protocol,
                    destination: &r.destination.to_string(),
                    gateway: &r.gateway.to_string(),
                    request_type: r.request_type,
                })
            );
        } else {
            assert_eq!(expect, None);
        }
    }

    #[tokio::test]
    async fn build_request_unknown_dest() {
        run_build_request(
            "127.0.0.1",
            "1.2.3.4:80",
            XdsAddressType::Workload(XdsWorkload {
                uid: "cluster1//v1/Pod/default/my-pod".to_string(),
                addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 2])],
                ..Default::default()
            }),
            Some(ExpectedRequest {
                protocol: Protocol::TCP,
                destination: "1.2.3.4:80",
                gateway: "1.2.3.4:80",
                request_type: RequestType::Passthrough,
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn build_request_known_dest_remote_node_tcp() {
        run_build_request(
            "127.0.0.1",
            "127.0.0.2:80",
            XdsAddressType::Workload(XdsWorkload {
                uid: "cluster1//v1/Pod/ns/test-tcp".to_string(),
                name: "test-tcp".to_string(),
                namespace: "ns".to_string(),
                addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 2])],
                tunnel_protocol: XdsProtocol::None as i32,
                node: "remote-node".to_string(),
                ..Default::default()
            }),
            Some(ExpectedRequest {
                protocol: Protocol::TCP,
                destination: "127.0.0.2:80",
                gateway: "127.0.0.2:80",
                request_type: RequestType::Direct,
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn build_request_known_dest_remote_node_hbone() {
        run_build_request(
            "127.0.0.1",
            "127.0.0.2:80",
            XdsAddressType::Workload(XdsWorkload {
                uid: "cluster1//v1/Pod/ns/test-tcp".to_string(),
                name: "test-tcp".to_string(),
                namespace: "ns".to_string(),
                addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 2])],
                tunnel_protocol: XdsProtocol::Hbone as i32,
                node: "remote-node".to_string(),
                ..Default::default()
            }),
            Some(ExpectedRequest {
                protocol: Protocol::HBONE,
                destination: "127.0.0.2:80",
                gateway: "127.0.0.2:15008",
                request_type: RequestType::Direct,
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn build_request_known_dest_local_node_tcp() {
        run_build_request(
            "127.0.0.1",
            "127.0.0.2:80",
            XdsAddressType::Workload(XdsWorkload {
                uid: "cluster1//v1/Pod/ns/test-tcp".to_string(),
                name: "test-tcp".to_string(),
                namespace: "ns".to_string(),
                addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 2])],
                tunnel_protocol: XdsProtocol::None as i32,
                node: "local-node".to_string(),
                ..Default::default()
            }),
            Some(ExpectedRequest {
                protocol: Protocol::TCP,
                destination: "127.0.0.2:80",
                gateway: "127.0.0.2:80",
                request_type: RequestType::Direct,
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn build_request_known_dest_local_node_hbone() {
        run_build_request(
            "127.0.0.1",
            "127.0.0.2:80",
            XdsAddressType::Workload(XdsWorkload {
                uid: "cluster1//v1/Pod/ns/test-tcp".to_string(),
                name: "test-tcp".to_string(),
                namespace: "ns".to_string(),
                addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 2])],
                tunnel_protocol: XdsProtocol::Hbone as i32,
                node: "local-node".to_string(),
                ..Default::default()
            }),
            Some(ExpectedRequest {
                protocol: Protocol::HBONE,
                destination: "127.0.0.2:80",
                gateway: "127.0.0.2:15008",
                request_type: RequestType::Direct,
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn build_request_unknown_source() {
        run_build_request(
            "1.2.3.4",
            "127.0.0.2:80",
            XdsAddressType::Workload(XdsWorkload {
                uid: "cluster1//v1/Pod/default/my-pod".to_string(),
                addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 2])],
                ..Default::default()
            }),
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn build_request_source_waypoint() {
        run_build_request(
            "127.0.0.2",
            "127.0.0.1:80",
            XdsAddressType::Workload(XdsWorkload {
                uid: "cluster1//v1/Pod/default/my-pod".to_string(),
                addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 2])],
                waypoint: Some(xds::istio::workload::GatewayAddress {
                    destination: Some(xds::istio::workload::gateway_address::Destination::Address(
                        XdsNetworkAddress {
                            network: "".to_string(),
                            address: [127, 0, 0, 10].to_vec(),
                        },
                    )),
                    hbone_mtls_port: 15008,
                    hbone_single_tls_port: 15003,
                }),
                ..Default::default()
            }),
            // Even though source has a waypoint, we don't use it
            Some(ExpectedRequest {
                protocol: Protocol::TCP,
                destination: "127.0.0.1:80",
                gateway: "127.0.0.1:80",
                request_type: RequestType::Direct,
            }),
        )
        .await;
    }
    #[tokio::test]
    async fn build_request_destination_waypoint() {
        run_build_request(
            "127.0.0.1",
            "127.0.0.2:80",
            XdsAddressType::Workload(XdsWorkload {
                uid: "cluster1//v1/Pod/default/my-pod".to_string(),
                addresses: vec![Bytes::copy_from_slice(&[127, 0, 0, 2])],
                waypoint: Some(xds::istio::workload::GatewayAddress {
                    destination: Some(xds::istio::workload::gateway_address::Destination::Address(
                        XdsNetworkAddress {
                            network: "".to_string(),
                            address: [127, 0, 0, 10].to_vec(),
                        },
                    )),
                    hbone_mtls_port: 15008,
                    hbone_single_tls_port: 15003,
                }),
                ..Default::default()
            }),
            // Should use the waypoint
            Some(ExpectedRequest {
                protocol: Protocol::HBONE,
                destination: "127.0.0.2:80",
                gateway: "127.0.0.10:15008",
                request_type: RequestType::ToServerWaypoint,
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn build_request_destination_svc_waypoint() {
        run_build_request(
            "127.0.0.1",
            "127.0.0.3:80",
            XdsAddressType::Service(XdsService {
                addresses: vec![XdsNetworkAddress {
                    network: "".to_string(),
                    address: vec![127, 0, 0, 3],
                }],
                ports: vec![Port {
                    service_port: 80,
                    target_port: 8080,
                }],
                waypoint: Some(xds::istio::workload::GatewayAddress {
                    destination: Some(xds::istio::workload::gateway_address::Destination::Address(
                        XdsNetworkAddress {
                            network: "".to_string(),
                            address: [127, 0, 0, 10].to_vec(),
                        },
                    )),
                    hbone_mtls_port: 15008,
                    hbone_single_tls_port: 15003,
                }),
                ..Default::default()
            }),
            // Should use the waypoint
            Some(ExpectedRequest {
                protocol: Protocol::HBONE,
                destination: "127.0.0.3:80",
                gateway: "127.0.0.10:15008",
                request_type: RequestType::ToServerWaypoint,
            }),
        )
        .await;
    }

    #[derive(PartialEq, Debug)]
    struct ExpectedRequest<'a> {
        protocol: Protocol,
        destination: &'a str,
        gateway: &'a str,
        request_type: RequestType,
    }
}
