//! Agent 名称的文件系统安全校验
//!
//! 拒绝路径穿越、Windows 非法字符、Windows 保留名等，确保名称可作为文件夹名安全使用。
//! 从注册表主模块抽出，集中校验规则，便于独立测试与未来复用。

use super::types::RegistryError;

/// 名称安全校验（作为文件夹名）
///
/// 拒绝：空串/纯空白、超长（>255）、含路径分隔符或 `..`、Windows 非法字符、
/// Windows 保留名（CON/PRN/NUL/AUX/COM1-9/LPT1-9）、保留字 `default`。
pub(super) fn validate_name(name: &str) -> Result<(), RegistryError> {
    if name.trim().is_empty() {
        return Err(RegistryError::InvalidName("名称不能为空".to_string()));
    }
    if name.len() > 255 {
        return Err(RegistryError::InvalidName(
            "名称过长（超过 255 字符）".to_string(),
        ));
    }
    // 路径分隔符 / 路径穿越
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(RegistryError::InvalidName(
            "名称含非法路径字符（/ \\ 或 ..）".to_string(),
        ));
    }
    // Windows 非法字符
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(RegistryError::InvalidName(
            "名称含 Windows 非法字符".to_string(),
        ));
    }
    // Windows 保留名
    if is_windows_reserved(name) {
        return Err(RegistryError::InvalidName(
            "名称为 Windows 保留名".to_string(),
        ));
    }
    // 保留给默认 Agent
    if name == "default" {
        return Err(RegistryError::InvalidName("default 为保留名".to_string()));
    }
    Ok(())
}

/// 判断是否为 Windows 保留名（含带扩展名的情况，如 CON.txt）
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_uppercase();
    let core = stem.as_str();
    matches!(core, "CON" | "PRN" | "NUL" | "AUX")
        || core
            .strip_prefix("COM")
            .is_some_and(|s| matches!(s, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
        || core
            .strip_prefix("LPT")
            .is_some_and(|s| matches!(s, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_name("coder").is_ok());
        assert!(validate_name("my-agent").is_ok());
        assert!(validate_name("agent_42").is_ok());
        assert!(validate_name("翻译助手").is_ok());
    }

    #[test]
    fn validate_name_rejects_invalid() {
        // 空串 / 纯空白
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        // 路径分隔符 / 路径穿越
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a../b").is_err());
        // Windows 非法字符
        assert!(validate_name("a<b").is_err());
        assert!(validate_name("a:b").is_err());
        assert!(validate_name("a*b").is_err());
        assert!(validate_name("a|b").is_err());
        // Windows 保留名
        assert!(validate_name("CON").is_err());
        assert!(validate_name("con.txt").is_err());
        assert!(validate_name("COM1").is_err());
        assert!(validate_name("LPT9").is_err());
        // 保留字 default
        assert!(validate_name("default").is_err());
        // 超长
        let long = "a".repeat(256);
        assert!(validate_name(&long).is_err());
    }
}
