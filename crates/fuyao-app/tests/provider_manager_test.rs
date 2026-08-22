//! 供应商管理门面集成测试：写回落 global 层 → 经既有加载路径读回的端到端一致性
//!
//! 覆盖公开 API（`ProviderManager` 的供应商粒度 CRUD，模型内嵌载荷随供应商
//! 整体写入 / 替换）与真实文件系统的协作：临时 fuyao_home 隔离（字段注入，
//! 零环境变量），每条用例走「管理 API 写盘 → `load_config` 读回」闭环，钉死
//! 「写盘结果可被既有加载路径无损读回」的契约；另验证 toml patch 的格式保真
//! （目标段之外逐字保留）、.env 的单行级 upsert（用户手写行不动、只增改
//! 不删除）。

mod common;

use common::temp_agent_paths;
use fuyao_api::{Model, ModelCost, ModelLimit, ModelModalities, load_config};
use fuyao_app::{ProviderAdminError, ProviderManager, ProviderModelSpec, ProviderSpec};

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

/// 构造供应商写回载荷（不含密钥与模型）
fn spec(name: &str, base_url: Option<&str>) -> ProviderSpec {
    ProviderSpec {
        name: name.to_string(),
        base_url: base_url.map(str::to_string),
        api_key_env_var: None,
        api_key: None,
        models: Vec::new(),
    }
}

/// 构造模型条目载荷（指定 id，字段用最小合法模型）
fn model_entry(id: &str) -> ProviderModelSpec {
    ProviderModelSpec {
        id: id.to_string(),
        model: sample_model(),
    }
}

// ===== 创建：写盘 → 加载路径读回语义一致 =====

/// 创建供应商（含内嵌模型与密钥）后，经既有加载路径读回，字段语义与写入载荷一致
#[test]
fn create_provider_then_reload_matches_spec() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    manager
        .create_provider(
            "deepseek",
            ProviderSpec {
                name: "DeepSeek".to_string(),
                base_url: Some("https://api.deepseek.com".to_string()),
                api_key_env_var: Some("MY_DEEPSEEK_KEY".to_string()),
                api_key: Some("sk-plain".to_string()),
                models: vec![model_entry("deepseek-v4-flash")],
            },
        )
        .unwrap();

    // 经既有加载路径（三层合并加载）读回 global 层
    let config = load_config(&paths).unwrap().unwrap();
    let provider = &config.providers["deepseek"];
    assert_eq!(provider.name, "DeepSeek");
    assert_eq!(
        provider.options.base_url.as_deref(),
        Some("https://api.deepseek.com")
    );
    // toml 只写指针（用户自设变量名），不写明文
    assert!(provider.options.api_key.is_none());
    assert_eq!(provider.api_key_env_vars, vec!["MY_DEEPSEEK_KEY"]);

    let model = &provider.models["deepseek-v4-flash"];
    assert_eq!(model.name, "deepseek-v4-flash");
    assert_eq!(model.limit.context, 128000);
    assert_eq!(model.limit.input, Some(120000));
    assert_eq!(model.limit.output, 8192);
    assert_eq!(model.cost.input, Some(1.0));
    assert_eq!(model.cost.output, Some(2.0));
    assert_eq!(model.cost.cache, Some(0.2));
    assert_eq!(model.reasoning_efforts, vec!["low", "high"]);

    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert_eq!(env, "MY_DEEPSEEK_KEY=sk-plain\n");
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

/// 无密钥载荷：不落指针键、不碰 .env
#[test]
fn create_provider_without_key_writes_no_pointer_no_env() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    manager
        .create_provider("fresh", spec("全新供应商", None))
        .unwrap();

    let config = load_config(&paths).unwrap().unwrap();
    assert!(
        config.providers["fresh"].api_key_env_vars.is_empty(),
        "未提供变量名时不落指针键"
    );
    assert!(
        !paths.fuyao_home.join(".env").exists(),
        "未提供明文时不创建 .env"
    );
}

/// .env 目标变量已有手写值：直接覆盖更新（upsert 语义），其他手写行与注释不动
#[test]
fn create_provider_overwrites_existing_env_line_directly() {
    let (paths, _home) = temp_agent_paths();
    std::fs::write(
        paths.fuyao_home.join(".env"),
        "# 手写\nUSER_OWN_VAR=keep\nMY_DEEPSEEK_KEY=handwritten\n",
    )
    .unwrap();
    let manager = ProviderManager::new(paths.clone());

    manager
        .create_provider(
            "deepseek",
            ProviderSpec {
                api_key_env_var: Some("MY_DEEPSEEK_KEY".to_string()),
                api_key: Some("sk-managed".to_string()),
                ..spec("DeepSeek", None)
            },
        )
        .unwrap();

    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert_eq!(
        env, "# 手写\nUSER_OWN_VAR=keep\nMY_DEEPSEEK_KEY=sk-managed\n",
        "目标行覆盖，其他行逐字保留"
    );
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

// ===== 更新：字段 patch + models 整表替换 =====

/// 更新供应商：name 覆盖、base_url 清除后 options 段随之消失、models 整表
/// 替换为载荷（未携带的模型消失、载荷 id 即落盘键——模型 id 可变更）、段内
/// 用户手写的未知字段保留
#[test]
fn update_provider_replaces_models_and_patches_fields() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider(
            "deepseek",
            ProviderSpec {
                base_url: Some("https://old.example.com".to_string()),
                models: vec![
                    model_entry("deepseek-v4-flash"),
                    model_entry("deepseek-chat"),
                ],
                ..spec("DeepSeek", None)
            },
        )
        .unwrap();
    // 用户在段内手写的未知字段（管理面不认知，patch 须保留）
    let toml_path = paths.fuyao_home.join("fuyao.toml");
    let raw = std::fs::read_to_string(&toml_path).unwrap();
    let patched = raw.replace(
        "name = \"DeepSeek\"",
        "name = \"DeepSeek\"\nunknown_field = \"保留我\"",
    );
    std::fs::write(&toml_path, patched).unwrap();

    // 完整期望状态：改名 + 清除 base_url + 模型列表换成改名后的单模型
    manager
        .update_provider(
            "deepseek",
            ProviderSpec {
                name: "深度求索".to_string(),
                base_url: None,
                api_key_env_var: None,
                api_key: None,
                models: vec![model_entry("deepseek-v4-flash-renamed")],
            },
        )
        .unwrap();

    let config = load_config(&paths).unwrap().unwrap();
    let provider = &config.providers["deepseek"];
    assert_eq!(provider.name, "深度求索");
    assert!(
        provider.options.base_url.is_none(),
        "base_url = None 清除该项"
    );
    assert!(
        !provider.models.contains_key("deepseek-v4-flash"),
        "载荷未携带的模型随整表替换消失"
    );
    assert!(
        !provider.models.contains_key("deepseek-chat"),
        "载荷未携带的模型随整表替换消失"
    );
    assert!(
        provider.models.contains_key("deepseek-v4-flash-renamed"),
        "载荷 id 即落盘键（模型 id 可变更）"
    );
    let toml = std::fs::read_to_string(&toml_path).unwrap();
    assert!(
        toml.contains("unknown_field = \"保留我\""),
        "段内手写未知字段保留：{toml}"
    );
}

/// 空模型列表：models 键整体移除
#[test]
fn update_provider_with_empty_models_removes_models_key() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider(
            "deepseek",
            ProviderSpec {
                models: vec![model_entry("deepseek-v4-flash")],
                ..spec("DeepSeek", None)
            },
        )
        .unwrap();

    manager
        .update_provider("deepseek", spec("DeepSeek", None))
        .unwrap();

    let config = load_config(&paths).unwrap().unwrap();
    assert!(
        config.providers["deepseek"].models.is_empty(),
        "空载荷移除全部模型"
    );
    let toml = std::fs::read_to_string(paths.fuyao_home.join("fuyao.toml")).unwrap();
    assert!(
        !toml.contains("[providers.deepseek.models"),
        "models 键整体移除：{toml}"
    );
}

/// 更新供应商携带明文：.env upsert 进自设变量、toml 指针所见即所得（单值
/// 覆盖，手写追加的多指针被替换）、段内残留的 options.api_key 明文被移除
#[test]
fn update_provider_with_api_key_migrates_plaintext_to_env() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider("deepseek", spec("DeepSeek", None))
        .unwrap();

    // 模拟用户手写：toml 段内残留明文 + 追加了第二个指针变量
    let toml_path = paths.fuyao_home.join("fuyao.toml");
    let raw = std::fs::read_to_string(&toml_path).unwrap();
    let patched = raw.replace(
        "[providers.deepseek]",
        "[providers.deepseek]\noptions = { api_key = \"sk-legacy\" }\napi_key_env_vars = [\"OLD_KEY\", \"USER_OWN_VAR\"]",
    );
    std::fs::write(&toml_path, patched).unwrap();

    manager
        .update_provider(
            "deepseek",
            ProviderSpec {
                api_key_env_var: Some("MY_DEEPSEEK_KEY".to_string()),
                api_key: Some("sk-new-key".to_string()),
                ..spec("DeepSeek", None)
            },
        )
        .unwrap();

    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert!(env.contains("MY_DEEPSEEK_KEY=sk-new-key"));
    let toml = std::fs::read_to_string(&toml_path).unwrap();
    assert!(!toml.contains("sk-legacy"), "toml 内残留明文被移除");
    let config = load_config(&paths).unwrap().unwrap();
    assert_eq!(
        config.providers["deepseek"].api_key_env_vars,
        vec!["MY_DEEPSEEK_KEY"],
        "指针所见即所得：单值覆盖，手写多指针被替换"
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

/// 删除供应商：toml 段级联删（含模型）、.env 是用户持有资产不动（密钥行保留）
#[test]
fn delete_provider_cascades_models_but_keeps_env() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());
    // 预置用户手写 .env 内容（其他变量 + 注释 + 密钥行）
    std::fs::write(
        paths.fuyao_home.join(".env"),
        "# 手写注释\nUSER_OWN_VAR=keep-me\nMY_DEEPSEEK_KEY=sk-old\n",
    )
    .unwrap();
    manager
        .create_provider(
            "deepseek",
            ProviderSpec {
                api_key_env_var: Some("MY_DEEPSEEK_KEY".to_string()),
                models: vec![model_entry("deepseek-v4-flash")],
                ..spec("DeepSeek", None)
            },
        )
        .unwrap();

    manager.delete_provider("deepseek").unwrap();

    // toml：供应商段（含模型）消失，文件保留（还有其他内容时不整体删文件）
    let config = load_config(&paths).unwrap();
    assert!(
        config.is_none() || !config.unwrap().providers.contains_key("deepseek"),
        "删除后经加载路径读不到该供应商"
    );
    // .env：逐字节不动（变量与值均归用户持有）
    let env = std::fs::read_to_string(paths.fuyao_home.join(".env")).unwrap();
    assert_eq!(
        env, "# 手写注释\nUSER_OWN_VAR=keep-me\nMY_DEEPSEEK_KEY=sk-old\n",
        "删除不触碰 .env"
    );
}

/// 删除不存在的供应商报 NotFound
#[test]
fn delete_provider_missing_errors_not_found() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths);

    let err = manager.delete_provider("ghost").unwrap_err();
    assert!(matches!(err, ProviderAdminError::NotFound(_)));
}

// ===== 模型载荷：随供应商写入的特殊形态 =====

/// 含价格梯度的模型随创建写入后可无损读回（整数与浮点价格都保留）
#[test]
fn create_provider_with_tiers_model_round_trips() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

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
        .create_provider(
            "aliyun",
            ProviderSpec {
                models: vec![ProviderModelSpec {
                    id: "qwen3.6-plus".to_string(),
                    model,
                }],
                ..spec("阿里云百炼", None)
            },
        )
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
fn create_provider_with_dotted_model_id_round_trips() {
    let (paths, _home) = temp_agent_paths();
    let manager = ProviderManager::new(paths.clone());

    manager
        .create_provider(
            "aliyun",
            ProviderSpec {
                models: vec![model_entry("qwen3.6-plus")],
                ..spec("阿里云百炼", None)
            },
        )
        .unwrap();

    let config = load_config(&paths).unwrap().unwrap();
    assert!(
        config.providers["aliyun"]
            .models
            .contains_key("qwen3.6-plus"),
        "点号 id 引号键读回命中"
    );
}

// ===== 校验 fail-loud =====

/// 非法 id / 必填字段缺失 / 明文无变量名 / 变量名非法 / 模型 id 非法 /
/// limit.context 非正整数 / 载荷内模型 id 重复在写入前拦下（文件不动）
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
    // 明文无变量名（明文缺 .env 落点）
    assert!(matches!(
        manager.create_provider(
            "ok",
            ProviderSpec {
                api_key: Some("sk-plain".to_string()),
                ..spec("OK", None)
            }
        ),
        Err(ProviderAdminError::Invalid(_))
    ));
    // 变量名字符集非法
    assert!(matches!(
        manager.create_provider(
            "ok",
            ProviderSpec {
                api_key_env_var: Some("1BAD-NAME".to_string()),
                api_key: Some("sk-plain".to_string()),
                ..spec("OK", None)
            }
        ),
        Err(ProviderAdminError::Invalid(_))
    ));
    // 模型 id 含 '/'（破坏 provider/model 复合 id）
    assert!(matches!(
        manager.create_provider(
            "ok",
            ProviderSpec {
                models: vec![model_entry("a/b")],
                ..spec("OK", None)
            }
        ),
        Err(ProviderAdminError::Invalid(_))
    ));
    // 模型 limit.context = 0（fail-loud：写盘结果必须可被配置加载读回）
    let mut bad = sample_model();
    bad.limit.context = 0;
    assert!(matches!(
        manager.create_provider(
            "ok",
            ProviderSpec {
                models: vec![ProviderModelSpec {
                    id: "m".to_string(),
                    model: bad,
                }],
                ..spec("OK", None)
            }
        ),
        Err(ProviderAdminError::Invalid(_))
    ));
    // 载荷内模型 id 重复（整表替换以 id 为键，静默覆盖不可接受）
    assert!(matches!(
        manager.create_provider(
            "ok",
            ProviderSpec {
                models: vec![model_entry("m"), model_entry("m")],
                ..spec("OK", None)
            }
        ),
        Err(ProviderAdminError::Invalid(_))
    ));

    // 全部失败入参都没碰文件
    let config = load_config(&paths).unwrap();
    assert!(config.is_none(), "失败入参不写盘");
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
