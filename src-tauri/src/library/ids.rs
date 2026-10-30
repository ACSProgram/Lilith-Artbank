//! 图片 id 列表的公共校验：正整数、按输入顺序去重、数量上限。
//!
//! 元数据批量写、集合、任务范围与能力查询共用；调用方只决定名词、上限与上限文案。

use std::collections::HashSet;

use crate::app::error::{AppError, AppResult};

pub(crate) fn unique_positive(values: &[i64], noun: &str) -> AppResult<Vec<i64>> {
    let mut result = Vec::with_capacity(values.len());
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if *value <= 0 {
            return Err(AppError::message(format!("{noun} id 无效")));
        }
        if seen.insert(*value) {
            result.push(*value);
        }
    }
    Ok(result)
}

pub(crate) fn ensure_max(len: usize, max: usize, message: String) -> AppResult<()> {
    if len > max {
        return Err(AppError::message(message));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_positive_and_keeps_first_occurrence() {
        assert_eq!(
            unique_positive(&[3, 1, 3, 2, 2], "图片").unwrap(),
            vec![3, 1, 2]
        );
        assert!(unique_positive(&[0], "图片").is_err());
        assert!(unique_positive(&[-1], "图片").is_err());
    }

    #[test]
    fn limit_is_inclusive() {
        assert!(ensure_max(3, 3, "太多".to_owned()).is_ok());
        assert!(ensure_max(4, 3, "太多".to_owned()).is_err());
    }
}
