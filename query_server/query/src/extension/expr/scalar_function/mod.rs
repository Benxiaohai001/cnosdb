mod duration_in;
#[cfg(test)]
mod example;
mod gapfill;
mod gauge;
mod gis;
mod interpolate;
mod locf;
mod state_at;
mod utils;

use std::sync::Arc;

use datafusion::error::DataFusionError;
use datafusion::logical_expr::ScalarFunctionImplementation;
use spi::query::function::FunctionMetadataManager;
use spi::QueryResult;

use super::ts_gen_func::TSGenFunc;

pub const TIME_WINDOW_GAPFILL: &str = "time_window_gapfill";
pub const LOCF: &str = "locf";
pub const INTERPOLATE: &str = "interpolate";
pub const DURATION_IN: &str = "duration_in";
pub const STATE_AT: &str = "state_at";

pub fn register_udfs(func_manager: &mut dyn FunctionMetadataManager) -> QueryResult<()> {
    // extend function...
    // eg.
    //   example::register_udf(func_manager)?;
    gapfill::register_udf(func_manager)?;
    locf::register_udf(func_manager)?;
    interpolate::register_udf(func_manager)?;
    gauge::register_udfs(func_manager)?;
    duration_in::register_udf(func_manager)?;
    state_at::register_udf(func_manager)?;
    gis::register_udfs(func_manager)?;
    TSGenFunc::register_all_udf(func_manager)?;
    Ok(())
}

pub(crate) fn unimplemented_scalar_impl(name: &'static str) -> ScalarFunctionImplementation {
    Arc::new(move |_| {
        Err(DataFusionError::NotImplemented(format!(
            "{name} is not yet implemented"
        )))
    })
}

#[cfg(test)]
mod tests {
    use spi::query::function::FunctionMetadataManager;

    use super::example;
    use crate::function::simple_func_manager::SimpleFunctionMetadataManager;

    #[tokio::test]
    async fn test_example() {
        let mut func_manager = SimpleFunctionMetadataManager::default();

        let expect_udf = example::register_udf(&mut func_manager);

        assert!(expect_udf.is_ok(), "register_udf error.");

        let expect_udf = expect_udf.unwrap();

        let result_udf = func_manager.udf(&expect_udf.name);

        assert!(result_udf.is_ok(), "not get result from func manager.");

        assert_eq!(&expect_udf, result_udf.unwrap().as_ref());
    }
}
