// Copyright (c) 2017-2019, Substratum LLC (https://substratum.net) and/or its affiliates. All rights reserved.

use actix::Recipient;
use actix::Syn;
use live_cores_package::LiveCoresPackage;
use std::borrow::Borrow;
use std::net::IpAddr;
use sub_lib::cryptde::CryptDE;
use sub_lib::cryptde::CryptData;
use sub_lib::cryptde::CryptdecError;
use sub_lib::cryptde::PlainData;
use sub_lib::dispatcher::Component;
use sub_lib::dispatcher::Endpoint;
use sub_lib::dispatcher::InboundClientData;
use sub_lib::hop::Hop;
use sub_lib::hopper::ExpiredCoresPackage;
use sub_lib::hopper::ExpiredCoresPackagePackage;
use sub_lib::logger::Logger;
use sub_lib::stream_handler_pool::TransmitDataMsg;

pub struct RoutingService {
    cryptde: &'static CryptDE,
    is_bootstrap_node: bool,
    to_proxy_client: Recipient<Syn, ExpiredCoresPackage>,
    to_proxy_server: Recipient<Syn, ExpiredCoresPackage>,
    to_neighborhood: Recipient<Syn, ExpiredCoresPackagePackage>,
    to_dispatcher: Recipient<Syn, TransmitDataMsg>,
    logger: Logger,
}

impl RoutingService {
    pub fn new(
        cryptde: &'static CryptDE,
        is_bootstrap_node: bool,
        to_proxy_client: Recipient<Syn, ExpiredCoresPackage>,
        to_proxy_server: Recipient<Syn, ExpiredCoresPackage>,
        to_neighborhood: Recipient<Syn, ExpiredCoresPackagePackage>,
        to_dispatcher: Recipient<Syn, TransmitDataMsg>,
    ) -> RoutingService {
        RoutingService {
            cryptde,
            is_bootstrap_node,
            to_proxy_client,
            to_proxy_server,
            to_neighborhood,
            to_dispatcher,
            logger: Logger::new("RoutingService"),
        }
    }

    pub fn route(&self, ibcd: InboundClientData) {
        self.logger.debug(format!(
            "Received {} bytes of InboundClientData from Dispatcher",
            ibcd.data.len()
        ));
        let sender_ip = ibcd.peer_addr.ip();
        let last_data = ibcd.last_data;
        let live_package = match self.decrypt_and_deserialize_lcp(ibcd) {
            Ok(package) => package,
            Err(_) => {
                // TODO what should we do here? (nothing is unbound --so we don't need to blow up-- but we can't send this package)
                return ();
            }
        };

        let next_hop = live_package.next_hop(self.cryptde.borrow());

        if self.should_route_data(next_hop.component) {
            self.route_data(sender_ip, next_hop, live_package, last_data);
        }
        ()
    }

    fn route_data(
        &self,
        sender_ip: IpAddr,
        next_hop: Hop,
        live_package: LiveCoresPackage,
        last_data: bool,
    ) {
        if next_hop.component == Component::Hopper {
            self.route_data_externally(live_package, last_data);
        } else {
            self.route_data_internally(next_hop.component, sender_ip, live_package)
        }
    }

    fn route_data_internally(
        &self,
        component: Component,
        sender_ip: IpAddr,
        live_package: LiveCoresPackage,
    ) {
        match component {
            Component::ProxyServer => {
                self.handle_endpoint(component, &self.to_proxy_server, live_package)
            }
            Component::ProxyClient => {
                self.handle_endpoint(component, &self.to_proxy_client, live_package)
            }
            Component::Neighborhood => {
                self.handle_ip_endpoint(component, &self.to_neighborhood, live_package, sender_ip)
            }
            component => {
                unimplemented!(
                    "Data targets {:?}, but we don't handle that yet: log and ignore",
                    component
                );
            }
        }
    }

    fn route_data_externally(&self, live_package: LiveCoresPackage, last_data: bool) {
        let transmit_msg = match self.to_transmit_data_msg(live_package, last_data) {
            // crashpoint - need to figure out how to bubble up different kinds of errors, or just log and return
            Err(_) => unimplemented!(),
            Ok(m) => m,
        };
        self.logger.debug(format!(
            "Relaying {}-byte LiveCoresPackage Dispatcher inside a TransmitDataMsg",
            transmit_msg.data.len()
        ));
        self.to_dispatcher
            .try_send(transmit_msg)
            .expect("Dispatcher is dead");
    }

    fn to_transmit_data_msg(
        &self,
        live_package: LiveCoresPackage,
        last_data: bool,
    ) -> Result<TransmitDataMsg, CryptdecError> {
        let (next_key, next_live_package) = match live_package.to_next_live(self.cryptde.borrow()) {
            // crashpoint - log error and return None?
            Err(_) => unimplemented!(),
            Ok(p) => p,
        };
        let next_live_package_ser = match serde_cbor::ser::to_vec(&next_live_package) {
            // crashpoint - log error and return None?
            Err(_) => unimplemented!(),
            Ok(p) => p,
        };
        let next_live_package_enc = match self
            .cryptde
            .encode(&next_key, &PlainData::new(&next_live_package_ser[..]))
        {
            // crashpoint - log error and return None?
            Err(_) => unimplemented!(),
            Ok(p) => p,
        };
        Ok(TransmitDataMsg {
            endpoint: Endpoint::Key(next_key),
            last_data,
            data: next_live_package_enc.data,
            sequence_number: None,
        })
    }

    fn should_route_data(&self, component: Component) -> bool {
        if component == Component::Neighborhood {
            true
        } else if self.is_bootstrap_node {
            self.logger.error(format!(
                "Request for Bootstrap Node to route data to {:?}: rejected",
                component
            ));
            false
        } else {
            true
        }
    }

    fn handle_endpoint(
        &self,
        component: Component,
        recipient: &Recipient<Syn, ExpiredCoresPackage>,
        live_package: LiveCoresPackage,
    ) {
        let expired_package = live_package.to_expired(self.cryptde.borrow());
        self.logger.trace(format!(
            "Forwarding ExpiredCoresPackage to {:?}: {:?}",
            component, expired_package
        ));
        recipient
            .try_send(expired_package)
            .expect(&format!("{:?} is dead", component))
    }

    fn handle_ip_endpoint(
        &self,
        component: Component,
        recipient: &Recipient<Syn, ExpiredCoresPackagePackage>,
        live_package: LiveCoresPackage,
        sender_ip: IpAddr,
    ) {
        let expired_package = live_package.to_expired(self.cryptde.borrow());
        let expired_package_package = ExpiredCoresPackagePackage {
            expired_cores_package: expired_package,
            sender_ip,
        };
        self.logger.trace(format!(
            "Forwarding ExpiredCoresPackagePackage to {:?}: {:?}",
            component, expired_package_package
        ));
        recipient
            .try_send(expired_package_package)
            .expect(&format!("{:?} is dead", component))
    }

    fn decrypt_and_deserialize_lcp(&self, ibcd: InboundClientData) -> Result<LiveCoresPackage, ()> {
        let decrypted_package = match self.cryptde.decode(&CryptData::new(&ibcd.data[..])) {
            Ok(package) => package,
            Err(e) => {
                self.logger
                    .error(format!("Couldn't decrypt CORES package: {:?}", e));
                return Err(());
            }
        };
        let live_package =
            match serde_cbor::de::from_slice::<LiveCoresPackage>(&decrypted_package.data[..]) {
                Ok(package) => package,
                Err(e) => {
                    self.logger
                        .error(format!("Couldn't deserialize CORES package: {}", e));
                    return Err(());
                }
            };
        return Ok(live_package);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix::msgs;
    use actix::Actor;
    use actix::Addr;
    use actix::Arbiter;
    use actix::System;
    use hopper::Hopper;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::thread;
    use sub_lib::cryptde::Key;
    use sub_lib::peer_actors::BindMessage;
    use sub_lib::route::Route;
    use sub_lib::route::RouteSegment;
    use test_utils::logging::init_test_logging;
    use test_utils::logging::TestLogHandler;
    use test_utils::recorder::make_peer_actors;
    use test_utils::recorder::make_peer_actors_from;
    use test_utils::recorder::make_recorder;
    use test_utils::recorder::Recorder;
    use test_utils::test_utils::cryptde;
    use test_utils::test_utils::route_to_proxy_client;
    use test_utils::test_utils::route_to_proxy_server;

    #[test]
    fn converts_live_message_to_expired_for_proxy_client() {
        let cryptde = cryptde();
        let component = Recorder::new();
        let component_recording_arc = component.get_recording();
        let component_awaiter = component.get_awaiter();
        let route = route_to_proxy_client(&cryptde.public_key(), cryptde);
        let payload = PlainData::new(&b"abcd"[..]);
        let lcp = LiveCoresPackage::new(
            route,
            cryptde.encode(&cryptde.public_key(), &payload).unwrap(),
        );
        let lcp_a = lcp.clone();
        let data_ser = PlainData::new(&serde_cbor::ser::to_vec(&lcp).unwrap()[..]);
        let data_enc = cryptde.encode(&cryptde.public_key(), &data_ser).unwrap();
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678").unwrap(),
            reception_port: None,
            sequence_number: None,
            last_data: false,
            is_clandestine: false,
            data: data_enc.data,
        };
        thread::spawn(move || {
            let system = System::new("converts_live_message_to_expired_for_proxy_client");
            let peer_actors = make_peer_actors_from(None, None, None, Some(component), None, None);
            let subject = Hopper::new(cryptde, false);
            let subject_addr: Addr<Syn, Hopper> = subject.start();
            subject_addr.try_send(BindMessage { peer_actors }).unwrap();

            subject_addr.try_send(inbound_client_data).unwrap();

            system.run();
        });
        component_awaiter.await_message_count(1);
        let component_recording = component_recording_arc.lock().unwrap();
        let record = component_recording.get_record::<ExpiredCoresPackage>(0);
        let expected_ecp = lcp_a.to_expired(cryptde);
        assert_eq!(*record, expected_ecp);
    }

    #[test]
    fn converts_live_message_to_expired_for_proxy_server() {
        let cryptde = cryptde();
        let component = Recorder::new();
        let component_recording_arc = component.get_recording();
        let component_awaiter = component.get_awaiter();
        let route = route_to_proxy_server(&cryptde.public_key(), cryptde);
        let payload = PlainData::new(&b"abcd"[..]);
        let lcp = LiveCoresPackage::new(
            route,
            cryptde.encode(&cryptde.public_key(), &payload).unwrap(),
        );
        let lcp_a = lcp.clone();
        let data_ser = PlainData::new(&serde_cbor::ser::to_vec(&lcp).unwrap()[..]);
        let data_enc = cryptde.encode(&cryptde.public_key(), &data_ser).unwrap();
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678").unwrap(),
            reception_port: None,
            last_data: false,
            is_clandestine: false,
            sequence_number: None,
            data: data_enc.data,
        };
        thread::spawn(move || {
            let system = System::new("converts_live_message_to_expired_for_proxy_server");
            let peer_actors = make_peer_actors_from(Some(component), None, None, None, None, None);
            let subject = Hopper::new(cryptde, false);
            let subject_addr: Addr<Syn, Hopper> = subject.start();
            subject_addr.try_send(BindMessage { peer_actors }).unwrap();

            subject_addr.try_send(inbound_client_data).unwrap();

            system.run();
        });
        component_awaiter.await_message_count(1);
        let component_recording = component_recording_arc.lock().unwrap();
        let record = component_recording.get_record::<ExpiredCoresPackage>(0);
        let expected_ecp = lcp_a.to_expired(cryptde);
        assert_eq!(*record, expected_ecp);
    }

    #[test]
    fn refuses_data_for_proxy_client_if_is_bootstrap_node() {
        init_test_logging();
        let cryptde = cryptde();
        let route = route_to_proxy_client(&cryptde.public_key(), cryptde);
        let payload = PlainData::new(&b"abcd"[..]);
        let lcp = LiveCoresPackage::new(
            route,
            cryptde.encode(&cryptde.public_key(), &payload).unwrap(),
        );
        let data_ser = PlainData::new(&serde_cbor::ser::to_vec(&lcp).unwrap()[..]);
        let data_enc = cryptde.encode(&cryptde.public_key(), &data_ser).unwrap();
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678").unwrap(),
            reception_port: None,
            last_data: false,
            is_clandestine: false,
            sequence_number: None,
            data: data_enc.data,
        };
        let system = System::new("refuses_data_for_proxy_client_if_is_bootstrap_node");
        let subject = Hopper::new(cryptde, true);
        let subject_addr: Addr<Syn, Hopper> = subject.start();
        let peer_actors = make_peer_actors();
        subject_addr.try_send(BindMessage { peer_actors }).unwrap();

        subject_addr.try_send(inbound_client_data).unwrap();

        Arbiter::system().try_send(msgs::SystemExit(0)).unwrap();
        system.run();
        TestLogHandler::new().exists_log_containing(
            "ERROR: RoutingService: Request for Bootstrap Node to route data to ProxyClient: rejected",
        );
    }

    #[test]
    fn refuses_data_for_proxy_server_if_is_bootstrap_node() {
        init_test_logging();
        let cryptde = cryptde();
        let route = route_to_proxy_server(&cryptde.public_key(), cryptde);
        let payload = PlainData::new(&b"abcd"[..]);
        let lcp = LiveCoresPackage::new(
            route,
            cryptde.encode(&cryptde.public_key(), &payload).unwrap(),
        );
        let data_ser = PlainData::new(&serde_cbor::ser::to_vec(&lcp).unwrap()[..]);
        let data_enc = cryptde.encode(&cryptde.public_key(), &data_ser).unwrap();
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678").unwrap(),
            reception_port: None,
            sequence_number: None,
            last_data: false,
            is_clandestine: false,
            data: data_enc.data,
        };
        let system = System::new("refuses_data_for_proxy_server_if_is_bootstrap_node");
        let subject = Hopper::new(cryptde, true);
        let subject_addr: Addr<Syn, Hopper> = subject.start();
        let peer_actors = make_peer_actors();
        subject_addr.try_send(BindMessage { peer_actors }).unwrap();

        subject_addr.try_send(inbound_client_data).unwrap();

        Arbiter::system().try_send(msgs::SystemExit(0)).unwrap();
        system.run();
        TestLogHandler::new().exists_log_containing(
            "ERROR: RoutingService: Request for Bootstrap Node to route data to ProxyServer: rejected",
        );
    }

    #[test]
    fn refuses_data_for_hopper_if_is_bootstrap_node() {
        init_test_logging();
        let cryptde = cryptde();
        let route = Route::new(
            vec![RouteSegment::new(
                vec![&cryptde.public_key(), &cryptde.public_key()],
                Component::Hopper,
            )],
            cryptde,
        )
        .unwrap();
        let payload = PlainData::new(&b"abcd"[..]);
        let lcp = LiveCoresPackage::new(
            route,
            cryptde.encode(&cryptde.public_key(), &payload).unwrap(),
        );
        let data_ser = PlainData::new(&serde_cbor::ser::to_vec(&lcp).unwrap()[..]);
        let data_enc = cryptde.encode(&cryptde.public_key(), &data_ser).unwrap();
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678").unwrap(),
            reception_port: None,
            last_data: false,
            is_clandestine: true,
            sequence_number: None,
            data: data_enc.data,
        };
        let system = System::new("refuses_data_for_hopper_if_is_bootstrap_node");
        let subject = Hopper::new(cryptde, true);
        let subject_addr: Addr<Syn, Hopper> = subject.start();
        let peer_actors = make_peer_actors();
        subject_addr.try_send(BindMessage { peer_actors }).unwrap();

        subject_addr.try_send(inbound_client_data).unwrap();

        Arbiter::system().try_send(msgs::SystemExit(0)).unwrap();
        system.run();
        TestLogHandler::new().exists_log_containing(
            "ERROR: RoutingService: Request for Bootstrap Node to route data to Hopper: rejected",
        );
    }

    #[test]
    fn accepts_data_for_neighborhood_if_is_bootstrap_node() {
        init_test_logging();
        let cryptde = cryptde();
        let mut route = Route::new(
            vec![RouteSegment::new(
                vec![&cryptde.public_key(), &cryptde.public_key()],
                Component::Neighborhood,
            )],
            cryptde,
        )
        .unwrap();
        route.shift(cryptde);
        let payload = PlainData::new(&b"abcd"[..]);
        let lcp = LiveCoresPackage::new(
            route,
            cryptde.encode(&cryptde.public_key(), &payload).unwrap(),
        );
        let data_ser = PlainData::new(&serde_cbor::ser::to_vec(&lcp).unwrap()[..]);
        let data_enc = cryptde.encode(&cryptde.public_key(), &data_ser).unwrap();
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678").unwrap(),
            reception_port: None,
            last_data: false,
            is_clandestine: true,
            sequence_number: None,
            data: data_enc.data,
        };
        let system = System::new("accepts_data_for_neighborhood_if_is_bootstrap_node");
        let subject = Hopper::new(cryptde, true);
        let subject_addr: Addr<Syn, Hopper> = subject.start();
        let (neighborhood, _, neighborhood_recording_arc) = make_recorder();
        let peer_actors = make_peer_actors_from(None, None, None, None, Some(neighborhood), None);
        subject_addr.try_send(BindMessage { peer_actors }).unwrap();

        subject_addr.try_send(inbound_client_data.clone()).unwrap();

        Arbiter::system().try_send(msgs::SystemExit(0)).unwrap();
        system.run();
        let neighborhood_recording = neighborhood_recording_arc.lock().unwrap();
        let message: &ExpiredCoresPackagePackage = neighborhood_recording.get_record(0);
        assert_eq!(
            message.clone().expired_cores_package.payload_data(),
            payload
        );
        assert_eq!(
            message.clone().sender_ip,
            IpAddr::from_str("1.2.3.4").unwrap()
        );
        TestLogHandler::new().exists_no_log_containing(
            "ERROR: RoutingService: Request for Bootstrap Node to route data to Neighborhood: rejected",
        );
    }

    #[test]
    fn rejects_data_for_non_neighborhood_component_if_is_bootstrap_node() {
        init_test_logging();
        let cryptde = cryptde();
        let mut route = Route::new(
            vec![RouteSegment::new(
                vec![&cryptde.public_key(), &cryptde.public_key()],
                Component::ProxyClient,
            )],
            cryptde,
        )
        .unwrap();
        route.shift(cryptde);
        let payload = PlainData::new(&b"abcd"[..]);
        let lcp = LiveCoresPackage::new(
            route,
            cryptde.encode(&cryptde.public_key(), &payload).unwrap(),
        );
        let data_ser = PlainData::new(&serde_cbor::ser::to_vec(&lcp).unwrap()[..]);
        let data_enc = cryptde.encode(&cryptde.public_key(), &data_ser).unwrap();
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678").unwrap(),
            reception_port: None,
            last_data: false,
            is_clandestine: true,
            sequence_number: None,
            data: data_enc.data,
        };
        let system =
            System::new("rejects_data_for_non_neighborhood_component_if_is_bootstrap_node");
        let subject = Hopper::new(cryptde, true);
        let subject_addr: Addr<Syn, Hopper> = subject.start();
        let (proxy_client, _, proxy_client_recording_arc) = make_recorder();
        let peer_actors = make_peer_actors_from(None, None, None, Some(proxy_client), None, None);
        subject_addr.try_send(BindMessage { peer_actors }).unwrap();

        subject_addr.try_send(inbound_client_data.clone()).unwrap();

        Arbiter::system().try_send(msgs::SystemExit(0)).unwrap();
        system.run();
        let proxy_client_recording = proxy_client_recording_arc.lock().unwrap();
        assert_eq!(proxy_client_recording.len(), 0);
        TestLogHandler::new().exists_log_containing(
            "ERROR: RoutingService: Request for Bootstrap Node to route data to ProxyClient: rejected",
        );
    }

    #[test]
    fn passes_on_inbound_client_data_not_meant_for_this_node() {
        let cryptde = cryptde();
        let dispatcher = Recorder::new();
        let dispatcher_recording_arc = dispatcher.get_recording();
        let dispatcher_awaiter = dispatcher.get_awaiter();
        let next_key = Key::new(&[65, 65, 65]);
        let route = Route::new(
            vec![RouteSegment::new(
                vec![&cryptde.public_key(), &next_key],
                Component::Neighborhood,
            )],
            cryptde,
        )
        .unwrap();
        let payload = PlainData::new(&b"abcd"[..]);
        let lcp = LiveCoresPackage::new(route, cryptde.encode(&next_key, &payload).unwrap());
        let lcp_a = lcp.clone();
        let data_ser = PlainData::new(&serde_cbor::ser::to_vec(&lcp).unwrap()[..]);
        let data_enc = cryptde.encode(&cryptde.public_key(), &data_ser).unwrap();
        let inbound_client_data = InboundClientData {
            peer_addr: SocketAddr::from_str("1.2.3.4:5678").unwrap(),
            reception_port: None,
            last_data: true,
            is_clandestine: false,
            sequence_number: None,
            data: data_enc.data,
        };
        thread::spawn(move || {
            let system = System::new("converts_live_message_to_expired_for_proxy_server");
            let peer_actors = make_peer_actors_from(None, Some(dispatcher), None, None, None, None);
            let subject = Hopper::new(cryptde, false);
            let subject_addr: Addr<Syn, Hopper> = subject.start();
            subject_addr.try_send(BindMessage { peer_actors }).unwrap();

            subject_addr.try_send(inbound_client_data).unwrap();

            system.run();
        });
        dispatcher_awaiter.await_message_count(1);
        let dispatcher_recording = dispatcher_recording_arc.lock().unwrap();
        let record = dispatcher_recording.get_record::<TransmitDataMsg>(0);
        let expected_lcp = lcp_a.to_next_live(cryptde).unwrap().1;
        let expected_lcp_ser = PlainData::new(&serde_cbor::ser::to_vec(&expected_lcp).unwrap());
        let expected_lcp_enc = cryptde.encode(&next_key, &expected_lcp_ser).unwrap();
        assert_eq!(
            *record,
            TransmitDataMsg {
                endpoint: Endpoint::Key(next_key.clone()),
                last_data: true,
                sequence_number: None,
                data: expected_lcp_enc.data,
            }
        );
    }
}
