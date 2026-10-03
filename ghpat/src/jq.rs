//! --jq 支持（规格 §7.3）：jaq 内存执行，无临时文件

use jaq_core::{load::Arena, load::File, load::Loader, Compiler, Ctx, RcIter};
use jaq_json::Val;
use serde_json::Value;

/// 对 JSON 值应用 jq 过滤器，返回全部输出值
pub fn apply(filter_src: &str, input: &Value) -> Result<Vec<Value>, String> {
    let program = File { code: filter_src, path: () };
    let loader = Loader::new(jaq_std::defs().chain(jaq_json::defs()));
    let arena = Arena::default();
    let modules = loader
        .load(&arena, program)
        .map_err(|errs| format!("jq 语法错误: {:?}", errs.iter().collect::<Vec<_>>()))?;
    let filter = Compiler::default()
        .with_funs(jaq_std::funs().chain(jaq_json::funs()))
        .compile(modules)
        .map_err(|errs| format!("jq 编译错误: {:?}", errs.iter().collect::<Vec<_>>()))?;
    let inputs = RcIter::new(core::iter::empty());
    let mut out = filter.run((Ctx::new([], &inputs), Val::from(input.clone())));
    let mut results = Vec::new();
    for item in out.by_ref() {
        match item {
            Ok(val) => {
                let v: Value = val.into();
                results.push(v);
            }
            Err(e) => return Err(format!("jq 运行错误: {e:?}")),
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn basic() {
        let v = json!({"a": [1, 2, 3]});
        let r = apply(".a | length", &v).unwrap();
        assert_eq!(r, vec![json!(3)]);
        let r = apply(".a[]", &v).unwrap();
        assert_eq!(r, vec![json!(1), json!(2), json!(3)]);
        assert!(apply(".a[", &v).is_err());
    }
}
