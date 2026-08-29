//! 工具定义类型
//!
//! 工具定义的中立形态：名称 / 描述 / 参数 schema 三要素。
//! 供应商适配层负责把中立形态编码为各自协议的 wire 形态后发送。

use std::collections::HashMap;

/// 工具参数属性定义
///
/// 定义单个参数的类型和描述信息。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolParameterProperty {
    /// 参数类型：string, integer, number, boolean, array, object
    #[serde(rename = "type")]
    pub kind: String,

    /// 参数描述
    pub description: String,

    /// 默认值
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,

    /// 枚举值列表
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,

    /// 数组元素类型定义
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<HashMap<String, serde_json::Value>>,
}

impl Default for ToolParameterProperty {
    fn default() -> Self {
        Self {
            kind: "string".to_string(),
            description: String::new(),
            default: None,
            enum_values: None,
            items: None,
        }
    }
}

/// 工具参数定义
///
/// 定义工具的所有参数，包括类型、必需参数等。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolParameters {
    /// 参数类型，固定为 object
    #[serde(rename = "type")]
    pub kind: String,

    /// 参数属性映射
    pub properties: HashMap<String, ToolParameterProperty>,

    /// 必需参数列表
    pub required: Vec<String>,
}

impl Default for ToolParameters {
    fn default() -> Self {
        Self {
            kind: "object".to_string(),
            properties: HashMap::new(),
            required: Vec::new(),
        }
    }
}

/// 工具定义
///
/// 中立形态：名称 / 描述 / 参数 schema 三要素平铺，
/// 供供应商适配层编码为各自协议形态。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    /// 工具名称，唯一标识
    pub name: String,

    /// 工具功能描述
    pub description: String,

    /// 参数定义
    pub parameters: ToolParameters,
}

impl ToolDefinition {
    /// 创建新的工具定义
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: ToolParameters::default(),
        }
    }

    /// 返回声明式构造器，用于链式描述参数（消除逐字段手搓 schema 字面量）
    ///
    /// 无参数工具用 [`new`](Self::new)；带参数工具用本方法：
    /// ```no_run
    /// # use fuyao_api::ToolDefinition;
    /// # use serde_json::json;
    /// ToolDefinition::builder("read", "读取文件")
    ///     .string("path", "文件路径").required()
    ///     .integer("limit", "最大行数").default(json!(500))
    ///     .build();
    /// ```
    pub fn builder(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> ToolDefinitionBuilder {
        ToolDefinitionBuilder::new(name, description)
    }
}

/// 工具定义构造器：声明式描述参数，消除逐字段手搓 [`ToolParameterProperty`] 字面量
///
/// 每个类型方法（[`string`](Self::string) / [`integer`](Self::integer) / ...）追加一个参数
/// 并返回 `Self`；紧随其后的修饰方法（[`default`](Self::default) /
/// [`enum_values`](Self::enum_values) / [`items`](Self::items) / [`required`](Self::required)）
/// 作用于**最近追加**的那个参数。
///
/// 构造器契约：修饰方法必须在某个参数方法之后调用，否则 panic（编程错误，立即暴露）。
///
/// 唯一入口是 [`ToolDefinition::builder`](ToolDefinition::builder)，内置工具与二次开发的
/// 自定义工具共用；链式调用经类型推断衔接，调用方无需按名导入本类型——
/// 全仓按名引用为零是预期形态，不构成死代码判据。
pub struct ToolDefinitionBuilder {
    def: ToolDefinition,
    /// 最近追加的参数名，供修饰方法定位目标参数
    last_param: Option<String>,
}

impl ToolDefinitionBuilder {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            def: ToolDefinition::new(name, description),
            last_param: None,
        }
    }

    /// 追加一个参数（显式指定类型 `kind`）
    pub fn param(mut self, name: &str, kind: &str, description: impl Into<String>) -> Self {
        self.def.parameters.properties.insert(
            name.to_string(),
            ToolParameterProperty {
                kind: kind.to_string(),
                description: description.into(),
                default: None,
                enum_values: None,
                items: None,
            },
        );
        self.last_param = Some(name.to_string());
        self
    }

    /// 追加 string 类型参数
    pub fn string(self, name: &str, description: impl Into<String>) -> Self {
        self.param(name, "string", description)
    }

    /// 追加 integer 类型参数
    pub fn integer(self, name: &str, description: impl Into<String>) -> Self {
        self.param(name, "integer", description)
    }

    /// 追加 number 类型参数
    pub fn number(self, name: &str, description: impl Into<String>) -> Self {
        self.param(name, "number", description)
    }

    /// 追加 boolean 类型参数
    pub fn boolean(self, name: &str, description: impl Into<String>) -> Self {
        self.param(name, "boolean", description)
    }

    /// 追加 array 类型参数
    pub fn array(self, name: &str, description: impl Into<String>) -> Self {
        self.param(name, "array", description)
    }

    /// 给最近追加的参数设默认值
    pub fn default(mut self, value: serde_json::Value) -> Self {
        let name = self.last_param_name();
        self.def
            .parameters
            .properties
            .get_mut(&name)
            .expect("default 必须在 param/string/integer/... 之后调用")
            .default = Some(value);
        self
    }

    /// 给最近追加的参数设枚举值
    pub fn enum_values(mut self, values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let name = self.last_param_name();
        self.def
            .parameters
            .properties
            .get_mut(&name)
            .expect("enum_values 必须在 param/string/integer/... 之后调用")
            .enum_values = Some(values.into_iter().map(Into::into).collect());
        self
    }

    /// 给最近追加的参数设数组元素类型（items，仅 array 参数用）
    pub fn items(mut self, items: HashMap<String, serde_json::Value>) -> Self {
        let name = self.last_param_name();
        self.def
            .parameters
            .properties
            .get_mut(&name)
            .expect("items 必须在 param/array 之后调用")
            .items = Some(items);
        self
    }

    /// 标记最近追加的参数为必填
    pub fn required(mut self) -> Self {
        let name = self.last_param_name();
        self.def.parameters.required.push(name);
        self
    }

    /// 构建工具定义
    pub fn build(self) -> ToolDefinition {
        self.def
    }

    /// 取最近追加的参数名（修饰方法定位目标用）
    fn last_param_name(&self) -> String {
        self.last_param
            .clone()
            .expect("default/enum_values/items/required 必须在 param/string/integer/... 之后调用")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_new_creates_named_tool() {
        let tool = ToolDefinition::new("read_file", "读取文件内容");
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description, "读取文件内容");
        assert_eq!(tool.parameters.kind, "object");
        assert!(tool.parameters.properties.is_empty());
        assert!(tool.parameters.required.is_empty());
    }

    #[test]
    fn tool_parameters_default_is_object() {
        let params = ToolParameters::default();
        assert_eq!(params.kind, "object");
        assert!(params.properties.is_empty());
        assert!(params.required.is_empty());
    }

    #[test]
    fn tool_parameter_property_default_is_string() {
        let prop = ToolParameterProperty::default();
        assert_eq!(prop.kind, "string");
        assert!(prop.description.is_empty());
    }

    /// ToolDefinition 序列化后能反序列化回等价结构（MCP 工具注入需要 Deserialize）
    #[test]
    fn tool_definition_roundtrip_serialize_deserialize() {
        let mut def = ToolDefinition::new("read", "读取文件");
        // 填入一个参数，验证嵌套结构（properties / required / enum）往返保真
        def.parameters.properties.insert(
            "path".to_string(),
            ToolParameterProperty {
                kind: "string".to_string(),
                description: "文件路径".to_string(),
                default: None,
                enum_values: Some(vec!["a".to_string(), "b".to_string()]),
                items: None,
            },
        );
        def.parameters.required.push("path".to_string());

        let json = serde_json::to_value(&def).expect("序列化失败");
        let back: ToolDefinition =
            serde_json::from_value(json).expect("反序列化失败（缺少 Deserialize）");

        assert_eq!(back.name, "read");
        assert_eq!(back.description, "读取文件");
        assert_eq!(back.parameters.required, vec!["path".to_string()]);
        let prop = back
            .parameters
            .properties
            .get("path")
            .expect("参数 path 应保留");
        assert_eq!(prop.kind, "string");
        assert_eq!(
            prop.enum_values.as_deref(),
            Some(&["a".to_string(), "b".to_string()][..])
        );
    }

    #[test]
    fn builder_constructs_required_and_default_params() {
        use serde_json::json;
        let def = ToolDefinition::builder("read", "读取文件")
            .string("path", "文件路径")
            .required()
            .integer("limit", "最大行数")
            .default(json!(500))
            .build();

        assert_eq!(def.name, "read");
        assert_eq!(def.parameters.required, vec!["path".to_string()]);
        let path = def
            .parameters
            .properties
            .get("path")
            .expect("path 参数应存在");
        assert_eq!(path.kind, "string");
        assert!(path.default.is_none());
        let limit = def
            .parameters
            .properties
            .get("limit")
            .expect("limit 参数应存在");
        assert_eq!(limit.kind, "integer");
        assert_eq!(limit.default, Some(json!(500)));
    }

    #[test]
    fn builder_supports_enum_and_items() {
        use serde_json::json;
        use std::collections::HashMap;
        let items = HashMap::from([
            ("type".to_string(), json!("object")),
            ("required".to_string(), json!(["id"])),
        ]);
        let def = ToolDefinition::builder("multi", "多模式工具")
            .string("mode", "模式")
            .enum_values(["fast", "deep"])
            .default(json!("fast"))
            .array("list", "列表")
            .items(items)
            .build();

        let mode = def
            .parameters
            .properties
            .get("mode")
            .expect("mode 参数应存在");
        assert_eq!(
            mode.enum_values.as_deref(),
            Some(&["fast".to_string(), "deep".to_string()][..])
        );
        assert_eq!(mode.default, Some(json!("fast")));
        let list = def
            .parameters
            .properties
            .get("list")
            .expect("list 参数应存在");
        assert_eq!(list.kind, "array");
        assert!(list.items.is_some());
    }
}
