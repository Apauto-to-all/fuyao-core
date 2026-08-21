//! 供应商管理门面集成测试：写回落 global 层 → 经既有加载路径读回的端到端一致性
//!
//! 覆盖公开 API（`ProviderManager` 的 provider / model 两级 CRUD）与真实文件
//! 系统的协作：临时 fuyao_home 隔离（字段注入，零环境变量），每条用例走
//! 「管理 API 写盘 → `load_config` 读回」闭环，钉死「写盘结果可被既有加载
//! 路径无损读回」的契约；另验证 toml patch 的格式保真（目标段之外逐字保留）
//! 与 .env 的单行级增量（用户手写行不动、冲突信号、删除同步清理）。

mod common;

use common::temp_agent_paths;
use fuyao_api::{Model, ModelCost, ModelLimit, ModelModalities, load_config};
use fuyao_app::{ProviderAdminError, ProviderManager, ProviderSpec};

/// 构造最小合法模型（limit.context 必填正整数）
fn sample_model() -> Model {
    Model {
        name: "deepseek-v4-flash".to_string(),
        cost: ModelCost {
            input: Some(1.0),
            output: Some(2.0),
            reasoning: None,
            cache: Some(0.2),
            tiers: Vec::new(),
        },
        limit: ModelLimit {
            context: 128000,
            input: Some(120000),
            output: 8192,
        },
        reasoning_efforts: vec!["low".to_string(), "high".to_string()],
        modalities: ModelModalities::default(),
    }
}

/// 构造供应商写回载荷（不含密钥）
fn spec(name: &str, base_url: Option<&str>) -> ProviderSpec {
    ProviderSpec {
        name: name.to_string(),
        base_url: base_url.map(str::to_string),
        api_key: None,
        overwrite_api_key: false,
    }
}

// ===== 创建：写盘 → 加载路径读回语义一致 =====

/// 创建供应商 + 模型后，经既有加载路径读回，字段语义与写入载荷一致
#[test]
fn create_provider_then_reload_matches_spec() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    let env_var = manager
        .create_provider(
            "deepseek",
            spec("DeepSeek", Some("https://api.deepseek.com")),
        )
        .unwrap();
    assert_eq!(
        env_var, "DEEPSEEK_API_KEY",
        "变量名按 {{id 大写}}_API_KEY 生成"
    );
    manager
        .create_model("deepseek", "deepseek-v4-flash", sample_model())
        .unwrap();

    // 经既有加载路径（三层合并加载）读回 global 层
    let config = load_config(&paths).unwrap().unwrap();
    let provider = &config.providers["deepseek"];
    assert_eq!(provider.name, "DeepSeek");
    assert_eq!(
        provider.options.base_url.as_deref(),
        Some("https://api.deepseek.com")
    );
    // toml 只写指针，不写明文
    assert!(provider.options.api_key.is_none());
    assert_eq!(provider.api_key_env_vars, vec!["DEEPSEEK_API_KEY"]);

    let model = &provider.models["deepseek-v4-flash"];
    assert_eq!(model.name, "deepseek-v4-flash");
    assert_eq!(model.limit.context, 128000);
    assert_eq!(model.limit.input, Some(120000));
    assert_eq!(model.limit.output, 8192);
    assert_eq!(model.cost.input, Some(1.0));
    assert_eq!(model.cost.output, Some(2.0));
    assert_eq!(model.cost.cache, Some(0.2));
    assert_eq!(model.reasoning_efforts, vec!["low", "high"]);
}

/// api_key 随创建写入 global 层 .env（单行追加），toml 侧只有指针
#[test]
fn create_provider_with_api_key_writes_env_line() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    manager
        .create_provider(
            "zhipu",
            ProviderSpec {
                name: "智谱".to_string(),
                base_url: None,
                api_key: Some("sk-plain-123".to_string()),
                overwrite_api_key: false,
            },
        )
        .unwrap();

    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert_eq!(env, "ZHIPU_API_KEY=sk-plain-123\n");

    let config = load_config(&paths).unwrap().unwrap();
    assert_eq!(
        config.providers["zhipu"].api_key_env_vars,
        vec!["ZHIPU_API_KEY"]
    );
    assert!(config.providers["zhipu"].options.api_key.is_none());
}

/// .env 不存在时创建连父目录一起建（首次写回无需手工准备目录）
#[test]
fn create_provider_bootstraps_missing_files() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    manager
        .create_provider("fresh", spec("全新供应商", None))
        .unwrap();

    assert!(paths.fuyao_home.join("fuyao.toml").is_file());
}

// ===== toml patch 保真：目标段之外逐字保留 =====

/// 预置带注释 / 未知字段 / 其他供应商的手写 global fuyao.toml，创建新供应商后
/// 目标段之外的注释、未知字段、其他段逐字保留
#[test]
fn create_provider_preserves_unrelated_toml_verbatim() {
    let (paths, _home) = temp_agent_paths();
    let handwritten = "# 我的全局配置\n[llm]\nrequest_timeout_secs = 300 # 手写注释\n\n# 手写供应商（管理 API 不触碰）\n[providers.aliyun]\nname = \"阿里云百炼\"\napi_key_env_vars = [\"DASHSCOPE_API_KEY\"]\nunknown_future_field = \"保留我\"\n";
    std::fs::write(paths.fuyao_home.join("fuyao.toml"), handwritten).unwrap();
    let manager = ProviderManager::new(paths.clone());

    manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap();

    let after = std::fs::read_to_string(paths.fuyao_home.join("fuyao.toml")).unwrap();
    assert!(
        after.contains("# 我的全局配置"),
        "目标段之外的注释保留：{after}"
    );
    assert!(after.contains("request_timeout_secs = 300 # 手写注释"));
    assert!(after.contains("# 手写供应商（管理 API 不触碰）"));
    assert!(after.contains("[providers.aliyun]"));
    assert!(
        after.contains("unknown_future_field = \"保留我\""),
        "未知字段保留"
    );
    assert!(after.contains("[providers.deepseek]"), "新段已写入");

    // 手写供应商与其他配置经加载路径仍然在
    let config = load_config(&paths).unwrap().unwrap();
    assert_eq!(config.llm.request_timeout_secs, 300);
    assert!(config.providers.contains_key("aliyun"));
    assert!(config.providers.contains_key("deepseek"));
}

// ===== 更新 =====

/// 更新供应商：name 覆盖、base_url 清除后 options 段随之消失；models 不动
#[test]
fn update_provider_patches_managed_fields_only() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider(
            "deepseek",
            spec("DeepSeek", Some("https://old.example.com")),
        )
        .unwrap();
    manager
        .create_model("deepseek", "deepseek-v4-flash", sample_model())
        .unwrap();

    // 完整期望状态：改名 + 清除 base_url，不带 api_key（不动 .env）
    manager
        .update_provider("deepseek", spec("深度求索", None))
        .unwrap();

    let config = load_config(&paths).unwrap().unwrap();
    let provider = &config.providers["deepseek"];
    assert_eq!(provider.name, "深度求索");
    assert!(
        provider.options.base_url.is_none(),
        "base_url = None 清除该项"
    );
    assert!(
        provider.models.contains_key("deepseek-v4-flash"),
        "更新供应商不动其模型"
    );
    assert_eq!(
        provider.api_key_env_vars,
        vec!["DEEPSEEK_API_KEY"],
        "指针保留"
    );
}

/// 更新供应商携带 api_key：明文写 .env、toml 残留的 options.api_key 明文被移除、
/// 指针去重追加（用户自加的变量保留）
#[test]
fn update_provider_with_api_key_migrates_plaintext_to_env() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap();

    // 模拟用户手写：toml 段内残留明文 + 自加了第二个指针变量
    let toml_path = paths.fuyao_home.join("fuyao.toml");
    let raw = std::fs::read_to_string(&toml_path).unwrap();
    let patched = raw.replace(
        "api_key_env_vars = [\"DEEPSEEK_API_KEY\"]",
        "api_key_env_vars = [\"DEEPSEEK_API_KEY\", \"USER_OWN_VAR\"]\noptions = { api_key = \"sk-legacy\" }",
    );
    std::fs::write(&toml_path, patched).unwrap();

    manager
        .update_provider(
            "deepseek",
            ProviderSpec {
                name: "DeepSeek".to_string(),
                base_url: None,
                api_key: Some("sk-new-key".to_string()),
                overwrite_api_key: false,
            },
        )
        .unwrap();

    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert!(env.contains("DEEPSEEK_API_KEY=sk-new-key"));
    let toml = std::fs::read_to_string(&toml_path).unwrap();
    assert!(!toml.contains("sk-legacy"), "toml 内残留明文被移除");
    assert!(!toml.contains("options"), "options 段空后整体移除");
    let config = load_config(&paths).unwrap().unwrap();
    assert_eq!(
        config.providers["deepseek"].api_key_env_vars,
        vec!["DEEPSEEK_API_KEY", "USER_OWN_VAR"],
        "约定变量去重保留，用户自加变量不动"
    );
}

/// 更新不存在的供应商报 NotFound
#[test]
fn update_provider_missing_errors_not_found() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    let err = manager
        .update_provider("ghost", spec("Ghost", None))
        .unwrap_err();
    assert!(matches!(err, ProviderAdminError::NotFound(_)));
}

/// 创建已存在的供应商报 AlreadyExists（id 不可改名，无静默覆盖）
#[test]
fn create_provider_duplicate_errors_already_exists() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap();

    let err = manager
        .create_provider("deepseek", spec("又一个", None))
        .unwrap_err();
    assert!(matches!(err, ProviderAdminError::AlreadyExists(_)));
}

// ===== 删除 =====

/// 删除供应商：段级联删（含模型）、.env 约定变量同步清理、用户手写变量与其他
/// 供应商逐字保留
#[test]
fn delete_provider_cascades_models_and_cleans_env() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    // 预置用户手写 .env 内容（其他变量 + 注释）
    std::fs::write(
        paths.fuyao_home.join(".env"),
        "# 手写注释\nUSER_OWN_VAR=keep-me\nDEEPSEEK_API_KEY=sk-old\n",
    )
    .unwrap();
    manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap();
    manager
        .create_model("deepseek", "deepseek-v4-flash", sample_model())
        .unwrap();

    manager.delete_provider("deepseek").unwrap();

    // toml：供应商段（含模型）消失，文件保留（还有其他内容时不整体删文件）
    let config = load_config(&paths).unwrap();
    assert!(
        config.is_none() || !config.unwrap().providers.contains_key("deepseek"),
        "删除后经加载路径读不到该供应商"
    );
    // .env：目标变量行删除，用户手写行与注释不动
    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert_eq!(env, "# 手写注释\nUSER_OWN_VAR=keep-me\n");
}

/// 删除不存在的供应商报 NotFound
#[test]
fn delete_provider_missing_errors_not_found() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths);

    let err = manager.delete_provider("ghost").unwrap_err();
    assert!(matches!(err, ProviderAdminError::NotFound(_)));
}

// ===== 模型粒度 CRUD =====

/// 模型 create / update / delete 全链路经加载路径读回
#[test]
fn model_crud_round_trips_through_load_path() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap();

    // 读侧闭包：经既有加载路径取目标模型的当前落盘状态
    let loaded = || load_config(&paths).unwrap().unwrap();
    let take_model = || loaded().providers["deepseek"].models["deepseek-v4-flash"].clone();

    // create
    manager
        .create_model("deepseek", "deepseek-v4-flash", sample_model())
        .unwrap();
    assert_eq!(take_model().limit.context, 128000);

    // update：整表替换（改价格与上下文）
    let mut updated = sample_model();
    updated.limit.context = 256000;
    updated.cost.input = Some(3.0);
    manager
        .update_model("deepseek", "deepseek-v4-flash", updated)
        .unwrap();
    let model = take_model();
    assert_eq!(model.limit.context, 256000);
    assert_eq!(model.cost.input, Some(3.0));

    // delete：模型消失，供应商保留
    manager
        .delete_model("deepseek", "deepseek-v4-flash")
        .unwrap();
    let config = loaded();
    assert!(
        !config.providers["deepseek"]
            .models
            .contains_key("deepseek-v4-flash")
    );
    assert!(config.providers.contains_key("deepseek"));
}

/// 含价格梯度的模型写入后可无损读回（整数与浮点价格都保留）
#[test]
fn create_model_with_tiers_round_trips() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider("aliyun", spec("阿里云百炼", None))
        .unwrap();

    let mut model = sample_model();
    model.name = "qwen3.6-plus".to_string();
    model.cost = ModelCost {
        input: Some(2.0),
        output: Some(12.0),
        reasoning: None,
        cache: Some(0.4),
        tiers: vec![fuyao_api::PriceTier {
            max_tokens: 256000,
            input: Some(2.0),
            output: Some(12.0),
            reasoning: None,
            cache: Some(0.4),
        }],
    };
    manager
        .create_model("aliyun", "qwen3.6-plus", model)
        .unwrap();

    let config = load_config(&paths).unwrap().unwrap();
    let loaded = &config.providers["aliyun"].models["qwen3.6-plus"];
    assert_eq!(loaded.cost.tiers.len(), 1);
    assert_eq!(loaded.cost.tiers[0].max_tokens, 256000);
    assert_eq!(loaded.cost.tiers[0].input, Some(2.0));
    assert_eq!(loaded.cost.tiers[0].cache, Some(0.4));
    assert_eq!(loaded.cost.input, Some(2.0));
}

/// 带点号的模型 id 落盘自动引号键，读回命中同一 id
#[test]
fn create_model_with_dotted_id_round_trips() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider("aliyun", spec("阿里云百炼", None))
        .unwrap();

    manager
        .create_model("aliyun", "qwen3.6-plus", sample_model())
        .unwrap();

    let config = load_config(&paths).unwrap().unwrap();
    assert!(
        config.providers["aliyun"]
            .models
            .contains_key("qwen3.6-plus"),
        "点号 id 引号键读回命中"
    );
}

/// 模型 id 已占用 / 目标供应商缺失 / 模型缺失的报错路径
#[test]
fn model_crud_error_paths() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    // 供应商不存在
    let err = manager
        .create_model("ghost", "m", sample_model())
        .unwrap_err();
    assert!(matches!(err, ProviderAdminError::NotFound(_)));

    manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap();
    manager
        .create_model("deepseek", "m", sample_model())
        .unwrap();

    // 模型 id 冲突
    let err = manager
        .create_model("deepseek", "m", sample_model())
        .unwrap_err();
    assert!(matches!(err, ProviderAdminError::AlreadyExists(_)));

    // 更新 / 删除不存在的模型
    let err = manager
        .update_model("deepseek", "absent", sample_model())
        .unwrap_err();
    assert!(matches!(err, ProviderAdminError::NotFound(_)));
    let err = manager.delete_model("deepseek", "absent").unwrap_err();
    assert!(matches!(err, ProviderAdminError::NotFound(_)));
}

// ===== 校验 fail-loud =====

/// 非法 id / 必填字段缺失 / limit.context 非正整数在写入前拦下（文件不动）
#[test]
fn invalid_inputs_fail_loud_without_touching_files() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    // 供应商 id 非法字符集 / 空
    assert!(matches!(
        manager.create_provider("bad.id", spec("x", None)),
        Err(ProviderAdminError::Invalid(_))
    ));
    assert!(matches!(
        manager.create_provider("", spec("x", None)),
        Err(ProviderAdminError::Invalid(_))
    ));
    // 供应商 name 空
    assert!(matches!(
        manager.create_provider("ok", spec("  ", None)),
        Err(ProviderAdminError::Invalid(_))
    ));
    // 模型 id 含 '/'（破坏 provider/model 复合 id）
    manager.create_provider("ok", spec("OK", None)).unwrap();
    assert!(matches!(
        manager.create_model("ok", "a/b", sample_model()),
        Err(ProviderAdminError::Invalid(_))
    ));
    // 模型 limit.context = 0（fail-loud：写盘结果必须可被配置加载读回）
    let mut bad = sample_model();
    bad.limit.context = 0;
    assert!(matches!(
        manager.create_model("ok", "m", bad),
        Err(ProviderAdminError::Invalid(_))
    ));

    // 全部失败入参都没碰文件（模型 id 校验通过前的 suppliers 段只含 ok）
    let config = load_config(&paths).unwrap().unwrap();
    assert_eq!(config.providers.len(), 1);
    assert!(config.providers["ok"].models.is_empty());
}

// ===== .env 冲突信号 =====

/// .env 目标变量已有手写值：未确认覆盖 → EnvKeyConflict 且两个文件都不动；
/// 确认覆盖 → 覆盖该行，其他行不动
#[test]
fn api_key_conflict_signals_without_partial_write() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    // 用户手写 .env：目标变量已有值 + 其他手写变量
    std::fs::write(
        paths.fuyao_home.join(".env"),
        "# 手写\nUSER_OWN_VAR=keep\nDEEPSEEK_API_KEY=handwritten\n",
    )
    .unwrap();

    let attempt = ProviderSpec {
        name: "DeepSeek".to_string(),
        base_url: None,
        api_key: Some("sk-managed".to_string()),
        overwrite_api_key: false,
    };
    let err = manager.create_provider("deepseek", attempt).unwrap_err();
    assert!(
        matches!(err, ProviderAdminError::EnvKeyConflict(ref var) if var == "DEEPSEEK_API_KEY"),
        "已有手写值应返回冲突信号：{err}"
    );

    // 冲突在写盘前拦下：toml 未被创建（不留「有指针、没密钥」的半状态）
    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert!(
        env.contains("DEEPSEEK_API_KEY=handwritten"),
        "冲突时 .env 不动"
    );
    let config = load_config(&paths).unwrap();
    assert!(config.is_none(), "冲突时 fuyao.toml 未写入");

    // 确认覆盖后重试：目标行被覆盖，其他行不动
    manager
        .create_provider(
            "deepseek",
            ProviderSpec {
                name: "DeepSeek".to_string(),
                base_url: None,
                api_key: Some("sk-managed".to_string()),
                overwrite_api_key: true,
            },
        )
        .unwrap();
    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert_eq!(
        env,
        "# 手写\nUSER_OWN_VAR=keep\nDEEPSEEK_API_KEY=sk-managed\n"
    );
}

// ===== 坏文件防线 =====

/// global fuyao.toml 已是非法 TOML 时写回直接报 TomlParse（不覆盖坏文件）
#[test]
fn write_back_rejects_unparsable_existing_toml() {
    let (paths, _home) = temp_agent_paths();
    std::fs::write(paths.fuyao_home.join("fuyao.toml"), "not [ valid toml").unwrap();
    let manager = ProviderManager::new(paths.clone());

    let err = manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap_err();
    assert!(matches!(err, ProviderAdminError::TomlParse(_)));
    // 坏文件原样保留（写回失败不扩大损伤）
    assert_eq!(
        std::fs::read_to_string(paths.fuyao_home.join("fuyao.toml")).unwrap(),
        "not [ valid toml"
    );
}

/// providers 段被写成标量时拒绝写回（段级结构错误 fail-loud，不盲写修复）
#[test]
fn write_back_rejects_non_table_providers_section() {
    let (paths, _home) = temp_agent_paths();
    std::fs::write(
        paths.fuyao_home.join("fuyao.toml"),
        "providers = \"oops\"\n",
    )
    .unwrap();
    let manager = ProviderManager::new(paths.clone());

    let err = manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap_err();
    assert!(matches!(err, ProviderAdminError::InvalidSection(_)));
}
