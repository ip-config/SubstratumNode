use neighborhood_lib::gossip::Gossip;
use neighborhood_lib::gossip::GossipNodeRecord;
use neighborhood_lib::neighborhood_database::NodeRecordInner;
use neighborhood_lib::neighborhood_database::NodeSignatures;
use sub_lib::cryptde::CryptDE;
use sub_lib::cryptde::Key;
use sub_lib::cryptde_null::CryptDENull;
use sub_lib::dispatcher::Component;
use sub_lib::hopper::IncipientCoresPackage;
use sub_lib::route::Route;
use sub_lib::route::RouteSegment;
use substratum_node::SubstratumNode;

pub struct GossipBuilder {
    node_info: Vec<GossipBuilderNodeInfo>,
    connection_pairs: Vec<(Key, Key)>,
}

impl GossipBuilder {
    pub fn new() -> GossipBuilder {
        GossipBuilder {
            node_info: vec![],
            connection_pairs: vec![],
        }
    }

    pub fn add_node(mut self, node: &SubstratumNode, is_bootstrap: bool, include_ip: bool) -> Self {
        self.node_info.push(GossipBuilderNodeInfo {
            node_record_inner: NodeRecordInner {
                public_key: node.public_key(),
                node_addr_opt: match include_ip {
                    true => Some(node.node_addr()),
                    false => None,
                },
                is_bootstrap_node: is_bootstrap,
                wallet: None,
                neighbors: vec![],
                version: 0,
            },
            cryptde: Box::new(CryptDENull::from(&node.public_key())),
        });
        self
    }

    pub fn add_fictional_node(mut self, node_record: NodeRecordInner) -> Self {
        let key = node_record.public_key.clone();
        self.node_info.push(GossipBuilderNodeInfo {
            node_record_inner: node_record,
            cryptde: Box::new(CryptDENull::from(&key)),
        });
        self
    }

    pub fn add_connection(mut self, from_key: &Key, to_key: &Key) -> Self {
        self.connection_pairs
            .push((from_key.clone(), to_key.clone()));
        self
    }

    pub fn build(self) -> Gossip {
        let mut node_records: Vec<GossipNodeRecord> = self
            .node_info
            .into_iter()
            .map(|node_info| {
                let signatures =
                    NodeSignatures::from(node_info.cryptde.as_ref(), &node_info.node_record_inner);
                GossipNodeRecord {
                    inner: node_info.node_record_inner,
                    signatures,
                }
            })
            .collect();

        self.connection_pairs.iter ().for_each (|pair_ref| {
            let from_key = pair_ref.0.clone ();
            let from_node_ref_opt = node_records.iter_mut ().find (|n| n.inner.public_key == from_key);
            let to_key = pair_ref.1.clone ();
            if let Some (from_node_ref) = from_node_ref_opt {
                from_node_ref.inner.neighbors.push (to_key);
            }
            else {
                panic! ("You directed that {:?} should be made a neighbor of {:?}, but {:?} was never added to the GossipBuilder",
                    to_key, from_key, from_key)
            }
        });
        Gossip { node_records }
    }

    pub fn build_cores_package(self, from: &Key, to: &Key) -> IncipientCoresPackage {
        let gossip = self.build();
        IncipientCoresPackage::new(
            Route::new(
                vec![RouteSegment::new(vec![from, to], Component::Neighborhood)],
                &CryptDENull::from(from),
            )
            .unwrap(),
            gossip,
            to,
        )
    }
}

struct GossipBuilderNodeInfo {
    node_record_inner: NodeRecordInner,
    cryptde: Box<CryptDE>,
}
