//! On-demand EC2 prices from the AWS Price List API, cached for a week.
//!
//! Prices are for Linux with shared tenancy and no pre-installed software: what an Amazon Linux
//! dev box pays on demand. The API is free and prices rarely change, so each (region, instance
//! type) pair is fetched once and kept in `<cache dir>/bracket/prices.json`.

use std::{
    collections::{BTreeSet, HashMap},
    fs, io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aws_sdk_pricing::{
    Client,
    types::{Filter, FilterType},
};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The Price List API only answers in a few regions, us-east-1 among them.
pub const API_REGION: &str = "us-east-1";

const TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What a price is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key {
    pub region: String,
    pub instance_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Cached {
    #[serde(flatten)]
    key: Key,
    usd_per_hour: f64,
    /// Seconds since the Unix epoch.
    fetched_at: u64,
}

/// The on-disk price cache.
pub struct Cache {
    path: PathBuf,
}

impl Cache {
    /// The cache under the platform's cache directory, if it has one.
    pub fn from_cache_dir() -> Option<Self> {
        dirs::cache_dir().map(|dir| Cache::at(dir.join("bracket").join("prices.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        Cache { path }
    }

    /// A missing or unreadable cache is just empty: the prices get fetched again.
    fn load(&self) -> Vec<Cached> {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    fn save(&self, entries: &[Cached]) -> io::Result<()> {
        self.path.parent().map_or(Ok(()), fs::create_dir_all)?;
        fs::write(&self.path, serde_json::to_string_pretty(entries)?)
    }
}

/// Returns the on-demand USD-per-hour price for each key, from the cache where it's under a
/// week old and from the API otherwise. Keys without a Linux on-demand price are left out.
pub async fn hourly(
    client: &Client,
    cache: Option<&Cache>,
    keys: BTreeSet<Key>,
    now: SystemTime,
) -> Result<HashMap<Key, f64>> {
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let fresh: Vec<Cached> = cache
        .map(Cache::load)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| now.saturating_sub(entry.fetched_at) < TTL.as_secs())
        .collect();
    let missing: Vec<Key> = keys
        .iter()
        .filter(|key| !fresh.iter().any(|entry| &entry.key == *key))
        .cloned()
        .collect();

    let mut tasks = tokio::task::JoinSet::new();
    for key in missing {
        let client = client.clone();
        tasks.spawn(async move {
            let price = fetch(&client, &key).await;
            (key, price)
        });
    }
    let fetched = tasks
        .join_all()
        .await
        .into_iter()
        .filter_map(|(key, price)| match price {
            Ok(Some(usd_per_hour)) => Some(Ok(Cached {
                key,
                usd_per_hour,
                fetched_at: now,
            })),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<Result<Vec<_>>>()?;

    let entries: Vec<Cached> = fresh.into_iter().chain(fetched).collect();
    // The cache only saves time, so failing to write it isn't worth failing the command.
    let _ = cache.map(|cache| cache.save(&entries));
    Ok(entries
        .into_iter()
        .filter(|entry| keys.contains(&entry.key))
        .map(|entry| (entry.key, entry.usd_per_hour))
        .collect())
}

async fn fetch(client: &Client, key: &Key) -> Result<Option<f64>> {
    let filters = [
        ("regionCode", key.region.as_str()),
        ("instanceType", key.instance_type.as_str()),
        ("operatingSystem", "Linux"),
        ("tenancy", "Shared"),
        ("preInstalledSw", "NA"),
        ("capacitystatus", "Used"),
        ("licenseModel", "No License required"),
        ("marketoption", "OnDemand"),
    ]
    .map(|(field, value)| {
        Filter::builder()
            .r#type(FilterType::TermMatch)
            .field(field)
            .value(value)
            .build()
            .expect("type, field, and value are all set")
    });
    let output = client
        .get_products()
        .service_code("AmazonEC2")
        .set_filters(Some(filters.to_vec()))
        .send()
        .await
        .map_err(Error::aws("looking up EC2 prices"))?;
    Ok(output
        .price_list()
        .iter()
        .find_map(|product| on_demand_usd(product)))
}

/// The hourly USD price in one Price List product document.
fn on_demand_usd(product: &str) -> Option<f64> {
    let product: serde_json::Value = serde_json::from_str(product).ok()?;
    product["terms"]["OnDemand"]
        .as_object()?
        .values()
        .filter_map(|term| term["priceDimensions"].as_object())
        .flat_map(|dimensions| dimensions.values())
        .filter(|dimension| dimension["unit"] == "Hrs")
        .find_map(|dimension| dimension["pricePerUnit"]["USD"].as_str()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_pricing::operation::get_products::GetProductsOutput;
    use aws_smithy_mocks::{RuleMode, mock, mock_client};

    /// A trimmed Price List document, shaped like the real one for t4g.small in us-east-2.
    fn product(usd: &str) -> String {
        format!(
            r#"{{"product":{{"attributes":{{"instanceType":"t4g.small","regionCode":"us-east-2"}}}},
                "serviceCode":"AmazonEC2",
                "terms":{{"OnDemand":{{"X.JRTCKXETXF":{{"priceDimensions":{{"X.JRTCKXETXF.6YS6EN2CT7":{{
                    "unit":"Hrs","pricePerUnit":{{"USD":"{usd}"}},
                    "description":"$0.0168 per On Demand Linux t4g.small Instance Hour"}}}}}}}},
                "Reserved":{{"X.R":{{"priceDimensions":{{"X.R.Q":{{"unit":"Quantity","pricePerUnit":{{"USD":"99"}}}}}}}}}}}}}}"#
        )
    }

    fn key(region: &str, instance_type: &str) -> Key {
        Key {
            region: region.into(),
            instance_type: instance_type.into(),
        }
    }

    fn secs(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn on_demand_hourly_prices_are_read_from_product_documents() {
        assert_eq!(on_demand_usd(&product("0.0168000000")), Some(0.0168));
        assert_eq!(on_demand_usd(r#"{"terms":{"Reserved":{}}}"#), None);
        assert_eq!(on_demand_usd(&product("not a number")), None);
        assert_eq!(on_demand_usd("not json"), None);
    }

    fn pricing_rule() -> aws_smithy_mocks::Rule {
        mock!(Client::get_products).then_compute_output(|req| {
            let field = |name: &str| {
                req.filters()
                    .iter()
                    .find(|filter| filter.field() == name)
                    .map(|filter| filter.value().to_string())
            };
            assert_eq!(req.service_code(), Some("AmazonEC2"));
            assert_eq!(field("operatingSystem").as_deref(), Some("Linux"));
            let price = match field("instanceType").as_deref() {
                Some("t4g.small") => Some("0.0168"),
                Some("c7i.8xlarge") => Some("1.428"),
                _ => None,
            };
            price
                .into_iter()
                .fold(GetProductsOutput::builder(), |output, usd| {
                    output.price_list(product(usd))
                })
                .build()
        })
    }

    #[tokio::test]
    async fn prices_are_fetched_once_then_served_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::at(dir.path().join("bracket").join("prices.json"));
        let rule = pricing_rule();
        let client = mock_client!(aws_sdk_pricing, RuleMode::MatchAny, [&rule]);
        let keys: BTreeSet<Key> = [
            key("us-east-2", "t4g.small"),
            key("us-east-1", "c7i.8xlarge"),
            key("us-east-1", "brand-new.type"),
        ]
        .into();

        let prices = hourly(&client, Some(&cache), keys.clone(), secs(1_000))
            .await
            .unwrap();
        assert_eq!(rule.num_calls(), 3);
        assert_eq!(prices.len(), 2);
        assert_eq!(prices[&key("us-east-2", "t4g.small")], 0.0168);
        assert_eq!(prices[&key("us-east-1", "c7i.8xlarge")], 1.428);

        // A day later only the unpriced type is asked about again.
        let again = hourly(&client, Some(&cache), keys, secs(1_000 + 86_400))
            .await
            .unwrap();
        assert_eq!(again, prices);
        assert_eq!(rule.num_calls(), 4);
    }

    #[tokio::test]
    async fn week_old_prices_are_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::at(dir.path().join("prices.json"));
        let rule = pricing_rule();
        let client = mock_client!(aws_sdk_pricing, RuleMode::MatchAny, [&rule]);
        let keys: BTreeSet<Key> = [key("us-east-2", "t4g.small")].into();

        hourly(&client, Some(&cache), keys.clone(), secs(0))
            .await
            .unwrap();
        hourly(&client, Some(&cache), keys, secs(TTL.as_secs()))
            .await
            .unwrap();

        assert_eq!(rule.num_calls(), 2);
        assert_eq!(cache.load().len(), 1);
    }

    #[tokio::test]
    async fn a_corrupt_or_missing_cache_just_means_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::at(dir.path().join("prices.json"));
        fs::write(dir.path().join("prices.json"), "{not json").unwrap();
        let rule = pricing_rule();
        let client = mock_client!(aws_sdk_pricing, RuleMode::MatchAny, [&rule]);
        let keys: BTreeSet<Key> = [key("us-east-2", "t4g.small")].into();

        let prices = hourly(&client, Some(&cache), keys.clone(), secs(0))
            .await
            .unwrap();
        assert_eq!(prices.len(), 1);

        let uncached = hourly(&client, None, keys, secs(0)).await.unwrap();
        assert_eq!(uncached, prices);
        assert_eq!(rule.num_calls(), 2);
    }

    #[tokio::test]
    async fn api_errors_fail_the_lookup() {
        let rule = mock!(Client::get_products).then_error(|| {
            aws_sdk_pricing::operation::get_products::GetProductsError::generic(
                aws_sdk_pricing::error::ErrorMetadata::builder()
                    .code("AccessDeniedException")
                    .message("no pricing:GetProducts")
                    .build(),
            )
        });
        let client = mock_client!(aws_sdk_pricing, RuleMode::MatchAny, [&rule]);
        let keys: BTreeSet<Key> = [key("us-east-2", "t4g.small")].into();

        let err = hourly(&client, None, keys, secs(0)).await.unwrap_err();

        assert!(err.to_string().contains("AccessDeniedException"), "{err}");
    }
}
