//! The broker's manifest, as the operator registers it with `PUT /v1/hosted/brokers`.
//!
//! No secret bindings: the Slack tokens come from the node's notify settings and arrive per call.

pub const MANIFEST_JSON: &str = r#"{
  "brokerId": "vam.notify",
  "trustTier": "TRUST_TIER_FIRST_PARTY",
  "secretBindings": [],
  "egressDomains": ["slack.com", "wss-primary.slack.com", "wss-backup.slack.com"],
  "notificationChannel": {
    "channel": "slack",
    "surface": "slack",
    "maxActionsPerMinute": 60
  },
  "tools": []
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::encoding::json_to_proto;
    use vam_spec::v1::CapabilityManifest;
    use vam_spec::validation::Validate;

    #[test]
    fn manifest_decodes_and_validates() {
        let m: CapabilityManifest =
            json_to_proto("vam.v1.CapabilityManifest", MANIFEST_JSON).expect("decode");
        m.validate().expect("valid");
        let c = m.notification_channel.expect("channel block");
        assert_eq!((c.channel.as_str(), c.surface.as_str()), ("slack", "slack"));
        assert!(m.tools.is_empty() && m.secret_bindings.is_empty());
    }
}
