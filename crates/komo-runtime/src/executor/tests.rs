//! 工具目录的验收：装、撤、按能力面渲染 schema；契约复验。

use std::sync::Arc;

use komo_kernel::traits::{OutputWriter, Tool};
use komo_kernel::types::delegate::{DelegateContract, SchemaMode};
use komo_kernel::types::plan::ExecutionPlan;
use komo_kernel::types::surface::AgentSurface;
use komo_kernel::types::tool::{ToolContext, ToolDefinition, ToolError, ToolExposure, ToolOutput};
use serde_json::json;

use super::{ToolExecutor, check_contract};

/// 只有名字和曝光方式的工具：目录测试不执行它。
#[derive(Debug)]
struct Named {
    name: &'static str,
    exposure: ToolExposure,
}

fn direct(name: &'static str) -> Arc<dyn Tool> {
    Arc::new(Named {
        name,
        exposure: ToolExposure::Direct,
    })
}

#[async_trait::async_trait]
impl Tool for Named {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "测试用".into(),
            parameters: json!({ "type": "object" }),
            doc: None,
        }
    }

    async fn prepare(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ExecutionPlan, ToolError> {
        unreachable!("目录测试不调")
    }

    async fn execute(
        &self,
        _plan: ExecutionPlan,
        _ctx: &ToolContext,
        _sink: &mut dyn OutputWriter,
    ) -> Result<ToolOutput, ToolError> {
        unreachable!("目录测试不调")
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }
}

fn names(definitions: Vec<ToolDefinition>) -> Vec<String> {
    definitions
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

/// 热重载撤掉的工具（照 pi 的 `hideTools`，`docs/mcp.md` §2.2）：目录里没了，新 Run 看不到；
/// 撤的原因留着给钩子放桩；同名的挂回来就又在了，原因随之清掉。
#[test]
fn a_withdrawn_tool_leaves_the_catalog_with_its_reason_and_comes_back_when_reinstalled() {
    let executor = ToolExecutor::new(vec![direct("bash")]);
    let surface = AgentSurface::new(["bash"]);

    let removed = executor.withdraw(vec![("bash".into(), "MCP 服务器 demo 已从配置移除".into())]);
    assert_eq!(removed, vec!["bash".to_string()]);
    assert!(!executor.tool_names().contains(&"bash".to_string()));
    assert!(executor.definitions_for(&surface).is_empty());
    assert_eq!(
        executor.withdrawn_reason("bash").as_deref(),
        Some("MCP 服务器 demo 已从配置移除")
    );

    assert_eq!(
        executor.install(vec![direct("bash")]),
        vec!["bash".to_string()]
    );
    assert_eq!(names(executor.definitions_for(&surface)), vec!["bash"]);
    assert!(executor.withdrawn().is_empty());
}

/// 已经有的名字不换：在跑的 Run 的能力面里那个名字指的就是它。
#[test]
fn installing_an_existing_name_changes_nothing() {
    let executor = ToolExecutor::new(vec![direct("read")]);
    assert!(executor.install(vec![direct("read")]).is_empty());
}

#[test]
fn the_catalog_renders_a_surface_in_the_surfaces_order() {
    let executor = ToolExecutor::new(vec![direct("read"), direct("write")]);
    assert_eq!(names(executor.catalog()), vec!["read", "write"]);

    // 交给模型的 schema 按**能力面**来：面外的工具一个都不出现，顺序听能力面的。
    let surface = AgentSurface::new(["write", "read"]);
    assert_eq!(
        names(executor.definitions_for(&surface)),
        vec!["write", "read"]
    );
    let surface = AgentSurface::new(["write"]);
    assert_eq!(names(executor.definitions_for(&surface)), vec!["write"]);
}

/// 只在 codemode 里调的工具（`docs/mcp.md` §3）留在能力面里（`tool://` 读得到），但不进交给
/// 模型的 schema；没有接 codemode 时服务器段也不列它。
#[test]
fn codemode_only_tools_stay_callable_but_out_of_the_schema() {
    let executor = ToolExecutor::new(vec![
        direct("read"),
        Arc::new(Named {
            name: "mcp__demo__bump",
            exposure: ToolExposure::Codemode,
        }),
    ]);
    let surface = AgentSurface::new(["read", "mcp__demo__bump"]);
    assert_eq!(names(executor.definitions_for(&surface)), vec!["read"]);
    assert_eq!(
        names(executor.callable_definitions_for(&surface)),
        vec!["read", "mcp__demo__bump"]
    );
    assert!(executor.namespaces_for(&surface).is_empty());
}

/// 没有 `bash` 时 `edit` / `write` 的 `then_run` 参数一并摘掉：模型看不见一个注定被拒的参数。
#[test]
fn then_run_is_dropped_from_the_schema_without_bash() {
    #[derive(Debug)]
    struct WithThenRun;

    #[async_trait::async_trait]
    impl Tool for WithThenRun {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "write".into(),
                description: "测试用".into(),
                parameters: json!({ "type": "object", "properties": { "then_run": {}, "path": {} } }),
                doc: None,
            }
        }
        async fn prepare(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ExecutionPlan, ToolError> {
            unreachable!()
        }
        async fn execute(
            &self,
            _plan: ExecutionPlan,
            _ctx: &ToolContext,
            _sink: &mut dyn OutputWriter,
        ) -> Result<ToolOutput, ToolError> {
            unreachable!()
        }
    }

    let executor = ToolExecutor::new(vec![Arc::new(WithThenRun), direct("bash")]);
    let has_then_run = |surface: &AgentSurface| {
        executor.definitions_for(surface)[0].parameters["properties"]
            .get("then_run")
            .is_some()
    };
    assert!(has_then_run(&AgentSurface::new(["write", "bash"])));
    assert!(!has_then_run(&AgentSurface::new(["write"])));
}

fn contract(mode: SchemaMode) -> DelegateContract {
    DelegateContract {
        schema: json!({
            "type": "object",
            "required": ["count"],
            "properties": { "count": { "type": "integer" } }
        }),
        mode,
    }
}

/// 契约复验：围栏与前后闲话都容忍，不合规的报出原因。
#[test]
fn check_contract_takes_the_json_out_of_the_reply_and_validates_it() {
    let contract = contract(SchemaMode::Strict);
    let (value, validation) =
        check_contract(&contract, "数完了：\n```json\n{\"count\": 3}\n```\n以上").unwrap();
    assert_eq!(value["count"], json!(3));
    assert!(validation.is_valid());

    assert!(check_contract(&contract, "没有 JSON").is_err());
    assert!(check_contract(&contract, "{\"files\": []}").is_err());
}
