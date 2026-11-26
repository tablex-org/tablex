//! Which MongoDB filters match (or may match) every document of a collection: the filters a change
//! must not run on without the user being told, like an empty one.
//!
//! The judgement is deliberately wide: a filter is dangerous when it is empty, when it is opaque
//! (`$where`, `$expr`, `$nor`: JavaScript or an expression the text alone does not bound), or when
//! every constraint in it is always true (`_id: {$exists: true}`, `_id: {$gte: MinKey}`, a
//! `$nin: []`, an `$or` with a branch that is itself always true or whose two branches are
//! complementary: `a = 1 or a != 1`, `a exists or a does not`, `a in S or a not in S`, `a > 5 or
//! a <= 5`). A wrong "dangerous" costs the user one more question; a wrong "fine" costs a collection.

use bson::{Bson, Document};

/// Why `filter` is dangerous, or `None` when it names the documents it applies to.
pub(crate) fn danger(filter: &Document) -> Option<String> {
    if let Some(operator) = opaque(filter) {
        return Some(format!(
            "the filter uses `{operator}`, which can match every document and cannot be checked before it runs"
        ));
    }
    if always_true(filter) {
        return Some(if filter.is_empty() {
            "the filter is empty, so it applies to every document of the collection".to_owned()
        } else {
            "the filter is true for every document of the collection".to_owned()
        });
    }
    None
}

/// The first opaque operator (`$where`, `$expr`, `$nor`) at any depth of `$and` / `$or` / `$nor`.
fn opaque(filter: &Document) -> Option<&'static str> {
    for (key, value) in filter {
        match key.as_str() {
            "$where" => return Some("$where"),
            "$expr" => return Some("$expr"),
            "$nor" => return Some("$nor"),
            "$and" | "$or" => {
                if let Bson::Array(clauses) = value {
                    for clause in clauses {
                        if let Bson::Document(clause) = clause {
                            if let Some(operator) = opaque(clause) {
                                return Some(operator);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Every constraint of `filter` is true for every document.
fn always_true(filter: &Document) -> bool {
    filter.iter().all(|(key, value)| constraint_is_always_true(key, value))
}

fn constraint_is_always_true(key: &str, value: &Bson) -> bool {
    match key {
        "$comment" => true,
        "$jsonSchema" => matches!(value, Bson::Document(schema) if schema.is_empty()),
        "$and" => match value {
            Bson::Array(clauses) => {
                !clauses.is_empty()
                    && clauses.iter().all(|clause| matches!(clause, Bson::Document(clause) if always_true(clause)))
            }
            _ => false,
        },
        "$or" => match value {
            Bson::Array(clauses) => {
                let documents: Vec<&Document> = clauses
                    .iter()
                    .filter_map(|clause| match clause {
                        Bson::Document(clause) => Some(clause),
                        _ => None,
                    })
                    .collect();
                documents.iter().any(|clause| always_true(clause)) || has_complementary_pair(&documents)
            }
            _ => false,
        },
        // `_id` is in every document: it exists, and nothing is below MinKey or above MaxKey.
        "_id" => id_predicate_is_always_true(value),
        _ if key.starts_with('$') => false,
        _ => match value {
            Bson::Document(operators) => operators.get("$nin").is_some_and(
                |set| matches!(set, Bson::Array(set) if set.is_empty()) && operators.len() == 1,
            ),
            _ => false,
        },
    }
}

fn id_predicate_is_always_true(value: &Bson) -> bool {
    let Bson::Document(operators) = value else {
        return false;
    };
    if operators.is_empty() || operators.keys().any(|key| !key.starts_with('$')) {
        return false;
    }
    operators.iter().all(|(operator, operand)| match operator.as_str() {
        "$exists" => truthy(operand),
        "$ne" => matches!(operand, Bson::Null),
        "$gte" => matches!(operand, Bson::MinKey),
        "$lte" => matches!(operand, Bson::MaxKey),
        "$nin" => matches!(operand, Bson::Array(set) if set.is_empty()),
        _ => false,
    })
}

fn truthy(value: &Bson) -> bool {
    match value {
        Bson::Boolean(on) => *on,
        Bson::Int32(n) => *n != 0,
        Bson::Int64(n) => *n != 0,
        Bson::Double(n) => *n != 0.0,
        _ => false,
    }
}

/// One condition on one field.
#[derive(Debug, Clone, PartialEq)]
enum Condition<'a> {
    Eq(&'a Bson),
    Ne(&'a Bson),
    Gt(&'a Bson),
    Gte(&'a Bson),
    Lt(&'a Bson),
    Lte(&'a Bson),
    In(&'a [Bson]),
    Nin(&'a [Bson]),
    Exists(bool),
}

/// `{field: condition}` as the field and its single condition; `None` for anything more involved.
fn single_condition(clause: &Document) -> Option<(&str, Condition<'_>)> {
    let mut entries = clause.iter().filter(|(key, _)| key.as_str() != "$comment");
    let (field, value) = entries.next()?;
    if entries.next().is_some() || field.starts_with('$') {
        return None;
    }
    let Bson::Document(operators) = value else {
        return Some((field, Condition::Eq(value)));
    };
    if operators.keys().next().is_none_or(|key| !key.starts_with('$')) {
        // A sub-document compared whole.
        return Some((field, Condition::Eq(value)));
    }
    if operators.len() != 1 {
        return None;
    }
    let (operator, operand) = operators.iter().next()?;
    let condition = match (operator.as_str(), operand) {
        ("$eq", operand) => Condition::Eq(operand),
        ("$ne", operand) => Condition::Ne(operand),
        ("$gt", operand) => Condition::Gt(operand),
        ("$gte", operand) => Condition::Gte(operand),
        ("$lt", operand) => Condition::Lt(operand),
        ("$lte", operand) => Condition::Lte(operand),
        ("$in", Bson::Array(set)) => Condition::In(set),
        ("$nin", Bson::Array(set)) => Condition::Nin(set),
        ("$exists", operand) => Condition::Exists(truthy(operand)),
        _ => return None,
    };
    Some((field, condition))
}

fn same_set(left: &[Bson], right: &[Bson]) -> bool {
    left.len() == right.len() && left.iter().all(|item| right.contains(item))
}

fn complementary(left: &Condition<'_>, right: &Condition<'_>) -> bool {
    use Condition::*;
    match (left, right) {
        (Eq(a), Ne(b)) | (Ne(a), Eq(b)) => a == b,
        (Gt(a), Lte(b)) | (Lte(a), Gt(b)) | (Gte(a), Lt(b)) | (Lt(a), Gte(b)) => a == b,
        (In(a), Nin(b)) | (Nin(a), In(b)) => same_set(a, b),
        (Exists(a), Exists(b)) => a != b,
        _ => false,
    }
}

fn has_complementary_pair(clauses: &[&Document]) -> bool {
    let conditions: Vec<(&str, Condition<'_>)> = clauses.iter().filter_map(|clause| single_condition(clause)).collect();
    conditions.iter().enumerate().any(|(index, (field, condition))| {
        conditions[index + 1..]
            .iter()
            .any(|(other, other_condition)| field == other && complementary(condition, other_condition))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn dangerous(filter: Document) -> bool {
        danger(&filter).is_some()
    }

    #[test]
    fn an_empty_filter_and_one_that_is_only_a_comment() {
        assert!(dangerous(doc! {}));
        assert!(dangerous(doc! {"$comment": "all of them"}));
        assert!(danger(&doc! {}).unwrap().contains("empty"));
    }

    #[test]
    fn opaque_operators_are_dangerous_wherever_they_sit() {
        for filter in [
            doc! {"$where": "this.a > 1"},
            doc! {"$expr": {"$gt": ["$a", 1]}},
            doc! {"$expr": true, "status": "x"},
            doc! {"$nor": [{"a": 1}]},
            doc! {"status": "x", "$and": [{"a": 1}, {"$where": "true"}]},
            doc! {"$or": [{"a": 1}, {"$and": [{"$expr": {"$eq": [1, 1]}}]}]},
        ] {
            let reason = danger(&filter).unwrap_or_else(|| panic!("{filter}"));
            assert!(reason.contains("`$"), "{reason}");
        }
    }

    #[test]
    fn always_true_constraints() {
        for filter in [
            doc! {"_id": {"$exists": true}},
            doc! {"_id": {"$exists": 1}},
            doc! {"_id": {"$ne": Bson::Null}},
            doc! {"_id": {"$gte": Bson::MinKey}},
            doc! {"_id": {"$lte": Bson::MaxKey}},
            doc! {"_id": {"$nin": []}},
            doc! {"a": {"$nin": []}},
            doc! {"$jsonSchema": {}},
            doc! {"$and": [{}, {"_id": {"$exists": true}}]},
            doc! {"$or": [{}, {"a": 1}]},
            doc! {"$or": [{"a": 1}, {"_id": {"$exists": true}}]},
            doc! {"_id": {"$exists": true}, "$comment": "x"},
        ] {
            assert!(dangerous(filter.clone()), "{filter}");
        }
    }

    #[test]
    fn a_complementary_or_is_dangerous() {
        for filter in [
            doc! {"$or": [{"a": 1}, {"a": {"$ne": 1}}]},
            doc! {"$or": [{"a": {"$ne": "x"}}, {"a": {"$eq": "x"}}]},
            doc! {"$or": [{"a": {"$exists": true}}, {"a": {"$exists": false}}]},
            doc! {"$or": [{"a": {"$in": [1, 2]}}, {"a": {"$nin": [2, 1]}}]},
            doc! {"$or": [{"a": {"$gt": 5}}, {"a": {"$lte": 5}}]},
            doc! {"$or": [{"a": {"$gte": 5}}, {"a": {"$lt": 5}}]},
            doc! {"$or": [{"b": 2}, {"a": 1}, {"a": {"$ne": 1}}]},
        ] {
            assert!(dangerous(filter.clone()), "{filter}");
        }
    }

    #[test]
    fn a_filter_that_names_documents_is_not_dangerous() {
        for filter in [
            doc! {"status": "open"},
            doc! {"_id": 5},
            doc! {"_id": {"$in": [1, 2]}},
            doc! {"_id": {"$exists": false}},
            doc! {"_id": {"$gt": 5}},
            doc! {"a": {"$exists": true}},
            doc! {"a": {"$ne": 1}},
            doc! {"a": {"$nin": [1]}},
            doc! {"_id": {"$exists": true}, "status": "open"},
            doc! {"$or": [{"a": 1}, {"b": 2}]},
            doc! {"$or": [{"a": 1}, {"a": 2}]},
            doc! {"$or": [{"a": 1}, {"a": {"$ne": 2}}]},
            doc! {"$or": [{"a": {"$in": [1]}}, {"a": {"$nin": [2]}}]},
            doc! {"$or": [{"a": {"$gt": 5}}, {"a": {"$lte": 6}}]},
            doc! {"$or": [{"a": 1}, {"b": {"$ne": 1}}]},
            doc! {"$and": [{"a": 1}, {"_id": {"$exists": true}}]},
            doc! {"$text": {"$search": "x"}},
            doc! {"a": {"$regex": "^x"}},
            doc! {"$jsonSchema": {"required": ["a"]}},
        ] {
            assert!(!dangerous(filter.clone()), "{filter}: {:?}", danger(&filter));
        }
    }
}
