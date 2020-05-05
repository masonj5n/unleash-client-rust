// Copyright 2020 Cognite AS
//! The primary interface for users of the library.
use std::collections::hash_map::HashMap;
use std::default::Default;
use std::sync::Arc;

use log::{debug, info, trace, warn};

use arc_swap::ArcSwapOption;

use crate::api::Feature;
use crate::context::Context;
use crate::http::HTTP;
use crate::strategy;

pub struct ClientBuilder<'a> {
    strategies: HashMap<String, &'a strategy::Strategy>,
}

impl<'a> ClientBuilder<'a> {
    pub fn into_client<C: http_client::HttpClient + Default>(
        self,
        app_name: &str,
        instance_id: &str,
        authorization: Option<&str>,
    ) -> Result<Client<'a, C>, http_client::Error> {
        Ok(Client {
            http: HTTP::new(
                app_name.into(),
                instance_id.into(),
                authorization.map(|s| s.to_owned()),
            )?,
            cached_state: ArcSwapOption::from(None),
            strategies: self.strategies,
        })
    }

    pub fn strategy(&mut self, name: &str, strategy: &'a strategy::Strategy) -> &mut Self {
        self.strategies.insert(name.into(), strategy);
        self
    }
}

impl<'a> Default for ClientBuilder<'a> {
    fn default() -> ClientBuilder<'a> {
        let mut result = ClientBuilder {
            strategies: Default::default(),
        };
        result
            .strategy("default", &strategy::default)
            .strategy("applicationHostname", &strategy::hostname)
            .strategy("default", &strategy::default)
            .strategy("gradualRolloutRandom", &strategy::random)
            .strategy("gradualRolloutSessionId", &strategy::session_id)
            .strategy("gradualRolloutUserId", &strategy::user_id)
            .strategy("remoteAddress", &strategy::remote_address)
            .strategy("userWithId", &strategy::user_with_id)
            .strategy("flexibleRollout", &strategy::flexible_rollout);
        result
    }
}

pub struct Client<'a, C: http_client::HttpClient> {
    http: HTTP<C>,
    // known strategies: strategy_name : memoiser
    strategies: HashMap<String, &'a strategy::Strategy>,
    // memoised state: feature_name: [callback, callback, ...]
    cached_state: ArcSwapOption<HashMap<String, Vec<Box<strategy::Evaluate>>>>,
}

impl<'a, C: http_client::HttpClient + std::default::Default> Client<'a, C> {
    pub fn new(
        app_name: &str,
        instance_id: &str,
        authorization: Option<&str>,
    ) -> Result<Self, http_client::Error> {
        let builder = ClientBuilder::default();
        Ok(Self {
            http: HTTP::new(
                app_name.into(),
                instance_id.into(),
                authorization.map(|s| s.to_owned()),
            )?,
            cached_state: ArcSwapOption::from(None),
            strategies: builder.strategies,
        })
    }

    pub fn is_enabled(&self, feature_name: &str, context: Option<&Context>, default: bool) -> bool {
        trace!(
            "is_enabled: feature {} default {}, context {:?}",
            feature_name,
            default,
            context
        );
        let cache = self.cached_state.load();
        let cache = if let Some(cache) = &*cache {
            cache
        } else {
            // No API state loaded
            trace!("is_enabled: No API state");
            return false;
        };
        if let Some(memos) = cache.get(feature_name) {
            let default_context: Context = Default::default();
            let context = context.unwrap_or(&default_context);
            for memo in memos {
                if memo(context) {
                    debug!(
                        "is_enabled: feature {} enabled by memo {:p}, context {:?}",
                        feature_name, memo, context
                    );
                    return true;
                } else {
                    trace!(
                        "is_enabled: feature {} not enabled by memo {:p}, context {:?}",
                        feature_name,
                        memo,
                        context
                    );
                }
            }
            false
        } else {
            trace!(
                "is_enabled: Unknown feature {}, using default {}",
                feature_name,
                default
            );
            default
        }
    }

    pub fn memoize(&mut self, features: Vec<Feature>) -> Result<(), Box<dyn std::error::Error>> {
        let mut new_cache: HashMap<String, Vec<Box<strategy::Evaluate>>> = HashMap::new();
        for feature in features {
            if !feature.enabled {
                let memos = vec![Box::new(_disabled) as Box<dyn Fn(&Context) -> bool>];
                new_cache.insert(feature.name.clone(), memos);
                continue;
            }
            // TODO add variant support
            let mut memos = vec![];
            for api_strategy in feature.strategies {
                if let Some(code_strategy) = self.strategies.get(&api_strategy.name) {
                    memos.push(code_strategy(api_strategy.parameters));
                }
                // Graceful degradation: ignore this unknown strategy.
                // TODO: add a logging layer and log it.
                // TODO: add a metrics layer and emit metrics for it.
            }
            new_cache.insert(feature.name.clone(), memos);
        }
        self.cached_state.store(Some(Arc::new(new_cache)));
        Ok(())
    }
}

fn _disabled(_: &Context) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::HashMap;
    use std::collections::hash_set::HashSet;
    use std::hash::BuildHasher;

    use maplit::hashmap;

    use super::{Client, ClientBuilder};
    use crate::api::{Feature, Features, Strategy};
    use crate::context::Context;
    use crate::strategy;

    #[test]
    fn test_memoization() {
        let _ = simple_logger::init();
        let f = Features {
            version: 1,
            features: vec![
                Feature {
                    description: "default".into(),
                    enabled: true,
                    created_at: None,
                    variants: None,
                    name: "default".into(),
                    strategies: vec![Strategy {
                        name: "default".into(),
                        parameters: None,
                    }],
                },
                Feature {
                    description: "userWithId".into(),
                    enabled: true,
                    created_at: None,
                    variants: None,
                    name: "userWithId".into(),
                    strategies: vec![Strategy {
                        name: "userWithId".into(),
                        parameters: Some(hashmap!["userIds".into()=>"present".into()]),
                    }],
                },
                Feature {
                    description: "userWithId+default".into(),
                    enabled: true,
                    created_at: None,
                    variants: None,
                    name: "userWithId+default".into(),
                    strategies: vec![
                        Strategy {
                            name: "userWithId".into(),
                            parameters: Some(hashmap!["userIds".into()=>"present".into()]),
                        },
                        Strategy {
                            name: "default".into(),
                            parameters: None,
                        },
                    ],
                },
                Feature {
                    description: "disabled".into(),
                    enabled: false,
                    created_at: None,
                    variants: None,
                    name: "disabled".into(),
                    strategies: vec![Strategy {
                        name: "default".into(),
                        parameters: None,
                    }],
                },
            ],
        };
        let mut c = Client::<http_client::native::NativeClient>::new("foo", "test", None).unwrap();
        c.memoize(f.features).unwrap();
        let present: Context = Context {
            user_id: Some("present".into()),
            ..Default::default()
        };
        let missing: Context = Context {
            user_id: Some("missing".into()),
            ..Default::default()
        };
        // unknown features should honour the default
        assert_eq!(false, c.is_enabled("unknown", None, false));
        assert_eq!(true, c.is_enabled("unknown", None, true));
        // default should be enabled, no context needed
        assert_eq!(true, c.is_enabled("default", None, false));
        // user present should be present on userWithId
        assert_eq!(true, c.is_enabled("userWithId", Some(&present), false));
        // user missing should not
        assert_eq!(false, c.is_enabled("userWithId", Some(&missing), false));
        // user missing should be present on userWithId+default
        assert_eq!(
            true,
            c.is_enabled("userWithId+default", Some(&missing), false)
        );
        // disabled should be disabled
        assert_eq!(false, c.is_enabled("disabled", None, true));
    }

    fn _reversed_uids<S: BuildHasher>(
        parameters: Option<HashMap<String, String, S>>,
    ) -> Box<strategy::Evaluate> {
        let mut uids: HashSet<String> = HashSet::new();
        if let Some(parameters) = parameters {
            if let Some(uids_list) = parameters.get("userIds") {
                for uid in uids_list.split(',') {
                    uids.insert(uid.chars().rev().collect());
                }
            }
        }
        Box::new(move |context: &Context| -> bool {
            context
                .user_id
                .as_ref()
                .map(|uid| uids.contains(uid))
                .unwrap_or(false)
        })
    }
    #[test]
    fn test_custom_strategy() {
        let _ = simple_logger::init();
        let mut builder = ClientBuilder::default();
        builder.strategy("reversed", &_reversed_uids);
        let mut client = builder
            .into_client::<http_client::native::NativeClient>("foo", "test", None)
            .unwrap();

        let f = Features {
            version: 1,
            features: vec![
                Feature {
                    description: "default".into(),
                    enabled: true,
                    created_at: None,
                    variants: None,
                    name: "default".into(),
                    strategies: vec![Strategy {
                        name: "default".into(),
                        parameters: None,
                    }],
                },
                Feature {
                    description: "reversed".into(),
                    enabled: true,
                    created_at: None,
                    variants: None,
                    name: "reversed".into(),
                    strategies: vec![Strategy {
                        name: "reversed".into(),
                        parameters: Some(hashmap!["userIds".into()=>"abc".into()]),
                    }],
                },
            ],
        };
        client.memoize(f.features).unwrap();
        let present: Context = Context {
            user_id: Some("cba".into()),
            ..Default::default()
        };
        let missing: Context = Context {
            user_id: Some("abc".into()),
            ..Default::default()
        };
        // user cba should be present on reversed
        assert_eq!(true, client.is_enabled("reversed", Some(&present), false));
        // user abc should not
        assert_eq!(false, client.is_enabled("reversed", Some(&missing), false));
        // adding custom strategies shouldn't disable built-in ones
        // default should be enabled, no context needed
        assert_eq!(true, client.is_enabled("default", None, false));
    }
}