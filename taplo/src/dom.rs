//! This module contains the DOM for TOML source.
//!
//! Nodes in the DOM tree are typed and contain their character offsets
//! this allows for inspecting values while knowing where they actually are.
//!
//! When constructed from the root (which is practically always),
//! the tree is semantically analyzed according to the TOML specification.
//! All the dotted keys and arrays of tables are also merged and collected
//! into tables and arrays. The order is always preserved when possible.
//!
//! The current DOM doesn't have comment or whitespace information directly exposed,
//! but these can be added anytime.
//!
//! The DOM is immutable right now, and only allows for semantic analysis,
//! but the ability to partially rewrite it is planned.
use crate::{
    syntax::{SyntaxElement, SyntaxKind, SyntaxKind::*, SyntaxNode, SyntaxToken},
    util::{unescape, StringExt},
};
use indexmap::IndexMap;
use rowan::TextRange;
use std::{hash::Hash, iter::FromIterator, mem};

/// Casting allows constructing DOM nodes from syntax nodes.
pub trait Cast: Sized {
    fn cast(element: SyntaxElement) -> Option<Self>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Node {
    Root(RootNode),
    Table(TableNode),
    Entry(EntryNode),
    Key(KeyNode),
    Value(ValueNode),
    Array(ArrayNode),
}

dom_node_from!(
    RootNode => Root,
    TableNode => Table,
    EntryNode => Entry,
    KeyNode => Key,
    ValueNode => Value,
    ArrayNode => Array
);

impl Cast for Node {
    fn cast(element: SyntaxElement) -> Option<Self> {
        match element.kind() {
            STRING
            | MULTI_LINE_STRING
            | STRING_LITERAL
            | MULTI_LINE_STRING_LITERAL
            | INTEGER
            | INTEGER_HEX
            | INTEGER_OCT
            | INTEGER_BIN
            | FLOAT
            | BOOL
            | DATE
            | INLINE_TABLE => ValueNode::cdom_inner(element).map(|v| Node::Value(v)),
            KEY => KeyNode::cast(element).map(|v| Node::Key(v)),
            VALUE => ValueNode::cast(element).map(|v| Node::Value(v)),
            TABLE_HEADER | TABLE_ARRAY_HEADER => TableNode::cast(element).map(|v| Node::Table(v)),
            ENTRY => EntryNode::cast(element).map(|v| Node::Entry(v)),
            ARRAY => ArrayNode::cast(element).map(|v| Node::Array(v)),
            ROOT => RootNode::cast(element).map(|v| Node::Root(v)),
            _ => None,
        }
    }
}

impl Node {
    pub fn text_range(&self) -> TextRange {
        match self {
            Node::Root(v) => v.text_range(),
            Node::Table(v) => v.text_range(),
            Node::Entry(v) => v.text_range(),
            Node::Key(v) => v.text_range(),
            Node::Value(v) => v.text_range(),
            Node::Array(v) => v.text_range(),
        }
    }

    pub fn kind(&self) -> SyntaxKind {
        match self {
            Node::Root(v) => v.kind(),
            Node::Table(v) => v.kind(),
            Node::Entry(v) => v.kind(),
            Node::Key(v) => v.kind(),
            Node::Value(v) => v.kind(),
            Node::Array(v) => v.kind(),
        }
    }
}

dom_common!(
    RootNode,
    TableNode,
    EntryNode,
    ArrayNode,
    IntegerNode,
    StringNode
);

/// The root of the DOM.
///
/// Constructing it will normalize all the dotted keys,
/// and merge all the tables that need to be merged,
/// and also creates arrays from array of tables.
/// And also semantically validates the tree according
/// to the TOML specification.
///
/// If any errors occur, the tree might be
/// missing entries, or will be completely empty.
///
/// Syntax errors are **not** reported, those have to
/// be checked before constructing the DOM.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RootNode {
    syntax: SyntaxNode,
    errors: Vec<Error>,
    entries: Entries,
}

impl RootNode {
    pub fn entries(&self) -> &Entries {
        &self.entries
    }

    pub fn into_entries(self) -> Entries {
        self.entries
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn errors(&self) -> &[Error] {
        &self.errors
    }
}

impl Cast for RootNode {
    fn cast(syntax: SyntaxElement) -> Option<Self> {
        if syntax.kind() != ROOT {
            return None;
        }

        // Syntax node of the root.
        let n = syntax.into_node().unwrap();

        // All the entries in the TOML document.
        // The key is their full path, including all parent tables.
        //
        // The contents of inline tables are not checked, and they are
        // treated like any other value.
        //
        // We allocate much more than we'll need, as descendants includes pretty much
        // every key and value, even inside arrays and inline tables,
        // but this will be more performant than allocating after each entry.
        let mut entries: IndexMap<KeyNode, EntryNode> =
            IndexMap::with_capacity(n.descendants().count());

        // Current table prefix for its entries
        let mut prefix: Option<KeyNode> = None;

        // We have to track which entry is defined
        // under which table, because TOML
        // forbids mixing top level tables with dotted keys,
        // which are otherwise technically the same.
        let mut tables: IndexMap<KeyNode, Vec<KeyNode>> = IndexMap::new();

        let mut errors = Vec::new();

        for child in n.children_with_tokens() {
            match child.kind() {
                TABLE_HEADER | TABLE_ARRAY_HEADER => {
                    let t = match TableNode::cast(child) {
                        None => continue,
                        Some(t) => t,
                    };

                    let mut key = match t
                        .syntax
                        .first_child()
                        .and_then(|n| KeyNode::cast(rowan::NodeOrToken::Node(n)))
                        .ok_or(Error::Spanned {
                            range: t.text_range(),
                            message: "table has no key".into(),
                        }) {
                        Ok(k) => k,
                        Err(err) => {
                            errors.push(err);
                            continue;
                        }
                    };

                    // We have to iterate due to arrays of tables :(
                    // The key hashes contain their indices, and we don't
                    // know the last index. And we cannot know because arrays of
                    // tables can be nested and we'd have to track all of them.
                    let existing_table = entries.iter().rev().find(|(k, _)| k.eq_keys(&key));

                    // The entries below still belong to this table,
                    // so we cannot skip its prefix, even on errors.
                    if let Some((existing_key, existing)) = existing_table {
                        let existing_table_array = match &existing.value {
                            ValueNode::Table(t) => t.is_part_of_array(),
                            _ => false,
                        };

                        if existing_table_array && !t.is_part_of_array() {
                            errors.push(Error::ExpectedTableArray {
                                target: existing.key().clone(),
                                key: key.clone(),
                            });
                        } else if !existing_table_array && t.is_part_of_array() {
                            errors.push(Error::ExpectedTableArray {
                                target: key.clone(),
                                key: existing.key().clone(),
                            });
                        } else if !existing_table_array && !t.is_part_of_array() {
                            errors.push(Error::DuplicateKey {
                                first: existing.key().clone(),
                                second: key.clone(),
                            });
                        } else {
                            key = key.with_index(existing_key.index + 1);
                            entries.insert(
                                key.clone(),
                                EntryNode {
                                    syntax: t.syntax.clone(),
                                    key: key.clone(),
                                    value: ValueNode::Table(t),
                                },
                            );
                        }
                    } else {
                        entries.insert(
                            key.clone(),
                            EntryNode {
                                syntax: t.syntax.clone(),
                                key: key.clone(),
                                value: ValueNode::Table(t),
                            },
                        );
                    }

                    prefix = Some(key);
                }
                ENTRY => {
                    let entry = match EntryNode::cast(child) {
                        None => continue,
                        Some(e) => e,
                    };

                    let insert_key = match &prefix {
                        None => entry.key().clone(),
                        Some(p) => {
                            match tables.get_mut(p) {
                                None => {
                                    let mut v = Vec::with_capacity(10); // A wild guess
                                    v.push(entry.key().clone());
                                    tables.insert(p.clone(), v);
                                }
                                Some(v) => {
                                    v.push(entry.key().clone());
                                }
                            }

                            entry.key().clone().with_prefix(p)
                        }
                    };

                    if let Some(existing) = entries.get(&insert_key) {
                        errors.push(Error::DuplicateKey {
                            first: existing.key().clone(),
                            second: entry.key().clone(),
                        });
                        continue;
                    }

                    entries.insert(insert_key, entry);
                }
                _ => {}
            }
        }

        if let Some(p) = prefix {
            if !tables.contains_key(&p) {
                tables.insert(p, Vec::new());
            }
        }

        // Look for mixed top level tables and dotted keys.
        // This is ugly as hell, but I couldn't bother.
        for (k, entries) in &tables {
            for entry in entries {
                for (k2, _) in &tables {
                    if k.index != k2.index || k == k2 || k2.key_count() < k.key_count() {
                        continue;
                    }

                    if k2.is_part_of(&entry.clone().with_prefix(k)) {
                        errors.push(Error::DuplicateKey {
                            first: k.clone(),
                            second: k2.clone(),
                        })
                    }
                }
            }
        }

        // Some additional checks for each entry
        let grouped_by_index = entries.iter().fold(
            Vec::<Vec<(&KeyNode, &EntryNode)>>::new(),
            |mut all, (k, e)| {
                if all.len() < k.index + 1 {
                    let mut v = Vec::with_capacity(entries.len());
                    v.push((k, e));
                    all.push(v);
                } else {
                    all[k.index].push((k, e));
                }

                all
            },
        );

        'outer: for (group_idx, group) in grouped_by_index.iter().enumerate() {
            for (i, (k, e)) in group.iter().enumerate() {
                // Look for regular sub-tables before arrays of tables
                if group_idx == 0 {
                    let is_table_array = match &e.value {
                        ValueNode::Table(t) => t.is_part_of_array(),
                        _ => false,
                    };

                    if !is_table_array {
                        let table_array = group.iter().skip(i).find(|(k2, e2)| match &e2.value {
                            ValueNode::Table(t) => k2.is_part_of(k) && t.is_part_of_array(),
                            _ => false,
                        });

                        if let Some((k2, _)) = table_array {
                            errors.push(Error::ExpectedTableArray {
                                target: (&**k2).clone(),
                                key: (&**k).clone(),
                            })
                        }
                    }
                }

                // We might do more checks if needed
                break 'outer;
            }
        }

        let mut final_entries = Entries::from_map(entries);

        // Otherwise we could show false errors.
        if errors.is_empty() {
            final_entries.merge(&mut errors);
            final_entries.normalize();
        }

        Some(Self {
            entries: final_entries,
            errors,
            syntax: n,
        })
    }
}

/// A table node is used for tables, arrays of tables,
/// and also inline tables.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableNode {
    syntax: SyntaxNode,

    /// Array of tables.
    array: bool,

    /// Pseudo-tables are made from dotted keys.
    /// These are actually not part of the parsed
    /// source.
    pseudo: bool,

    entries: Entries,
}

impl TableNode {
    pub fn into_entries(self) -> Entries {
        self.entries
    }

    pub fn entries(&self) -> &Entries {
        &self.entries
    }

    pub fn is_part_of_array(&self) -> bool {
        self.array
    }

    pub fn is_inline(&self) -> bool {
        match self.kind() {
            INLINE_TABLE => true,
            _ => false,
        }
    }

    pub fn is_pseudo(&self) -> bool {
        self.pseudo
    }
}

impl Cast for TableNode {
    fn cast(syntax: SyntaxElement) -> Option<Self> {
        match syntax.kind() {
            TABLE_HEADER | TABLE_ARRAY_HEADER => {
                let n = syntax.into_node().unwrap();

                let key = n
                    .first_child()
                    .and_then(|e| KeyNode::cast(rowan::NodeOrToken::Node(e)));

                if key.is_none() {
                    return None;
                }

                Some(Self {
                    entries: Entries::default(),
                    pseudo: false,
                    array: n.kind() == TABLE_ARRAY_HEADER,
                    syntax: n,
                })
            }
            // FIXME(recursion)
            INLINE_TABLE => Some(Self {
                entries: syntax
                    .as_node()
                    .unwrap()
                    .children_with_tokens()
                    .filter_map(|c| Cast::cast(c))
                    .collect(),
                array: false,
                pseudo: false,
                syntax: syntax.into_node().unwrap(),
            }),
            _ => None,
        }
    }
}

/// Newtype that adds features to the regular
/// index map, used by root and table nodes.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct Entries(Vec<EntryNode>);

impl Entries {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &EntryNode> {
        self.0.iter()
    }

    pub fn into_iter(self) -> impl Iterator<Item = EntryNode> {
        self.0.into_iter()
    }

    fn from_map(map: IndexMap<KeyNode, EntryNode>) -> Self {
        Entries(
            map.into_iter()
                .map(|(k, mut e)| {
                    e.key = k;
                    e
                })
                .collect(),
        )
    }

    /// Merges entries into tables, merges tables where possible,
    /// creates arrays from arrays of tables.
    ///
    /// Any errors are pushed into errors and the affected
    /// values are dropped.
    ///
    /// The resulting entries are not normalized
    /// and will still contain dotted keys.
    ///
    /// This function assumes that arrays of tables have correct
    /// indices in order without skips and will panic otherwise.
    /// It also doesn't care about table duplicates, and will happily merge them.
    fn merge(&mut self, errors: &mut Vec<Error>) {
        // The new entry keys all will have indices of 0 as arrays are merged.
        let mut new_entries: Vec<EntryNode> = Vec::with_capacity(self.0.len());

        // We try to merge or insert all entries.
        for mut entry in mem::take(&mut self.0) {
            // We don't care about the exact index after this point,
            // everything should be in the correct order.
            entry.key = entry.key.with_index(0);

            let mut should_insert = true;

            for existing_entry in &mut new_entries {
                // If false, the entry was already merged
                // or a merge failure happened.
                match Entries::merge_entry(existing_entry, &entry, errors) {
                    Ok(merged) => {
                        if merged {
                            should_insert = false;
                            break;
                        }
                    }
                    Err(err) => {
                        errors.push(err);
                        should_insert = false;
                        break;
                    }
                }
            }

            if should_insert {
                // Transform array of tables into array
                entry.value = match entry.value {
                    ValueNode::Table(mut t) => {
                        if t.array {
                            t.array = false;
                            ValueNode::Array(ArrayNode {
                                syntax: t.syntax.clone(),
                                items: vec![ValueNode::Table(t)],
                                tables: true,
                            })
                        } else {
                            ValueNode::Table(t)
                        }
                    }
                    v => v,
                };

                new_entries.push(entry);
            }
        }

        self.0 = new_entries;
    }

    /// Normalizes all dotted keys into nested
    /// pseudo-tables.
    fn normalize(&mut self) {
        let mut entries_list = vec![&mut self.0];

        while let Some(entries) = entries_list.pop() {
            for entry in entries.iter_mut() {
                entry.normalize();

                match &mut entry.value {
                    ValueNode::Array(a) => {
                        let mut inner_arrs = vec![a];

                        while let Some(arr) = inner_arrs.pop() {
                            for item in arr.items.iter_mut() {
                                match item {
                                    ValueNode::Array(a) => {
                                        inner_arrs.push(a);
                                    }
                                    ValueNode::Table(t) => {
                                        entries_list.push(&mut t.entries.0);
                                    }

                                    _ => {}
                                }
                            }
                        }
                    }
                    ValueNode::Table(t) => {
                        entries_list.push(&mut t.entries.0);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Tries to merge entries into each other,
    /// old will always have the final result.
    ///
    /// It expects arrays of tables to be in order.
    ///
    /// Returns Ok(true) on a successful merge.
    /// Returns Ok(false) if the entries shouldn't be merged.
    /// Returns Err(...) if the entries should be merged, but an error ocurred.
    fn merge_entry(
        old_entry: &mut EntryNode,
        new_entry: &EntryNode,
        errors: &mut Vec<Error>,
    ) -> Result<bool, Error> {
        let old_key = old_entry.key.clone();
        let new_key = new_entry.key.clone();

        // Try to merge new into old first
        if old_key.is_part_of(&new_key) {
            match &mut old_entry.value {
                // There should be no conflicts, and duplicates
                // should be handled before reaching this point.
                ValueNode::Table(t) => {
                    if t.is_inline() {
                        return Err(Error::InlineTable {
                            target: old_entry.key.clone(),
                            key: new_entry.key.clone(),
                        });
                    }

                    let mut to_insert = new_entry.clone();
                    to_insert.key = new_key.clone().without_prefix(&old_key);
                    t.entries.0.push(to_insert);

                    // FIXME(recursion)
                    // It shouldn't be a problem here, but I mark it anyway.
                    t.entries.merge(errors);

                    Ok(true)
                }
                ValueNode::Array(old_arr) => {
                    if !old_arr.tables {
                        return Err(Error::ExpectedTableArray {
                            target: old_entry.key.clone(),
                            key: new_entry.key.clone(),
                        });
                    }

                    let mut final_entry = new_entry.clone();

                    match &mut final_entry.value {
                        ValueNode::Table(new_t) => {
                            if old_key.eq_keys(&new_key) && new_t.array {
                                new_t.array = false;
                                old_arr.items.push(final_entry.value);
                                Ok(true)
                            } else {
                                match old_arr.items.last_mut().unwrap() {
                                    ValueNode::Table(arr_t) => {
                                        let mut to_insert = new_entry.clone();
                                        to_insert.key = new_key.clone().without_prefix(&old_key);

                                        arr_t.entries.0.push(to_insert);

                                        // FIXME(recursion)
                                        // It shouldn't be a problem here, but I mark it anyway.
                                        arr_t.entries.merge(errors);
                                        Ok(true)
                                    }
                                    _ => panic!("expected array of tables"),
                                }
                            }
                        }
                        ValueNode::Empty => panic!("empty value"),
                        _ => {
                            match old_arr.items.last_mut().unwrap() {
                                ValueNode::Table(arr_t) => {
                                    let mut to_insert = new_entry.clone();
                                    to_insert.key = new_key.clone().without_prefix(&old_key);

                                    arr_t.entries.0.push(to_insert);

                                    // FIXME(recursion)
                                    // It shouldn't be a problem here, but I mark it anyway.
                                    arr_t.entries.merge(errors);
                                    Ok(true)
                                }
                                _ => panic!("expected array of tables"),
                            }
                        }
                    }
                }
                ValueNode::Empty => panic!("empty value"),
                _ => Err(Error::ExpectedTable {
                    target: old_entry.key.clone(),
                    key: new_entry.key.clone(),
                }),
            }

        // Same but the other way around.
        } else if new_key.is_part_of(&old_key) {
            let mut new_old = new_entry.clone();

            match Entries::merge_entry(&mut new_old, &old_entry, errors) {
                Ok(merged) => {
                    if merged {
                        *old_entry = new_old;
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
                Err(e) => Err(e),
            }

        // They might still share a prefix,
        // in that case a pseudo-table must be created.
        } else {
            let common_count = old_entry.key().common_prefix_count(new_entry.key());

            if common_count > 0 {
                let common_prefix = old_entry.key().clone().outer(common_count);

                let mut a = old_entry.clone();
                a.key = a.key.without_prefix(&common_prefix);

                let mut b = new_entry.clone();
                b.key = b.key.without_prefix(&common_prefix);

                old_entry.key = common_prefix;
                old_entry.value = ValueNode::Table(TableNode {
                    syntax: old_entry.syntax.clone(),
                    array: false,
                    pseudo: true,
                    entries: Entries(vec![a, b]),
                });
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

impl FromIterator<EntryNode> for Entries {
    fn from_iter<T: IntoIterator<Item = EntryNode>>(iter: T) -> Self {
        let i = iter.into_iter();
        let hint = i.size_hint();

        let len = match hint.1 {
            None => hint.0,
            Some(l) => l,
        };

        let mut entries = Vec::with_capacity(len);

        for entry in i {
            entries.push(entry);
        }

        Entries(entries)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArrayNode {
    syntax: SyntaxNode,
    tables: bool,
    items: Vec<ValueNode>,
}

impl ArrayNode {
    pub fn items(&self) -> &[ValueNode] {
        &self.items
    }

    pub fn into_items(self) -> Vec<ValueNode> {
        self.items
    }
}

impl Cast for ArrayNode {
    fn cast(syntax: SyntaxElement) -> Option<Self> {
        match syntax.kind() {
            // FIXME(recursion)
            ARRAY => Some(Self {
                items: syntax
                    .as_node()
                    .unwrap()
                    .descendants_with_tokens()
                    .filter_map(|c| Cast::cast(c))
                    .collect(),
                tables: false,
                syntax: syntax.into_node().unwrap(),
            }),
            TABLE_ARRAY_HEADER => Some(Self {
                items: Vec::new(),
                tables: false,
                syntax: syntax.into_node().unwrap(),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryNode {
    syntax: SyntaxNode,
    key: KeyNode,
    value: ValueNode,
}

impl EntryNode {
    pub fn key(&self) -> &KeyNode {
        &self.key
    }

    pub fn value(&self) -> &ValueNode {
        &self.value
    }

    pub fn into_value(self) -> ValueNode {
        self.value
    }

    /// Turns a dotted key into nested pseudo-tables.
    fn normalize(&mut self) {
        while self.key.key_count() > 1 {
            let new_key = self.key.clone().prefix();
            let inner_key = self.key.clone().last();

            let value = mem::take(&mut self.value);

            // We have to keep track of it in the pseudo-table.
            let is_array_table = match &value {
                ValueNode::Table(t) => t.is_part_of_array(),
                _ => false,
            };

            let inner_entry = EntryNode {
                syntax: self.syntax.clone(),
                key: inner_key.clone(),
                value,
            };

            let mut entries = Entries(Vec::with_capacity(1));

            entries.0.push(inner_entry);

            self.value = ValueNode::Table(TableNode {
                syntax: inner_key.syntax.clone(),
                array: is_array_table,
                pseudo: true,
                entries,
            });
            self.key = new_key;
        }
    }
}

impl Cast for EntryNode {
    fn cast(element: SyntaxElement) -> Option<Self> {
        if element.kind() != ENTRY {
            None
        } else {
            let key = element
                .as_node()
                .unwrap()
                .first_child_or_token()
                .and_then(Cast::cast);

            if key.is_none() {
                return None;
            }

            let val = element
                .as_node()
                .unwrap()
                .first_child()
                .and_then(|k| k.next_sibling())
                .map(|n| rowan::NodeOrToken::Node(n))
                .and_then(Cast::cast);

            if val.is_none() {
                return None;
            }

            Some(Self {
                key: key.unwrap(),
                value: val.unwrap(),
                syntax: element.into_node().unwrap(),
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeyNode {
    syntax: SyntaxNode,

    // Hash and equality is based on only
    // the string values of the idents.
    idents: Vec<SyntaxToken>,

    // This also contributes to equality and hashes.
    //
    // It is only used to differentiate arrays of tables
    // during parsing.
    index: usize,
}

impl KeyNode {
    pub fn idents(&self) -> &[SyntaxToken] {
        &self.idents
    }

    pub fn key_count(&self) -> usize {
        self.idents.len()
    }

    /// Parts of a dotted key
    pub fn keys(&self) -> Vec<String> {
        self.keys_str()
            .into_iter()
            .map(ToString::to_string)
            .collect()
    }

    pub fn keys_str(&self) -> impl Iterator<Item = &str> {
        self.idents.iter().map(|t| {
            let mut s = t.text().as_str();

            // We have to check in case a quote
            // is in a string literal, we would otherwise
            // remove both.
            if s.starts_with("\"") {
                s = s.trim_start_matches("\"").trim_end_matches("\"");
            }

            if s.starts_with("'") {
                s = s.trim_start_matches("'").trim_end_matches("'");
            }

            s
        })
    }

    /// Full dotted key
    pub fn full_key(&self) -> String {
        self.keys().join(".")
    }

    pub fn kind(&self) -> SyntaxKind {
        self.syntax.kind()
    }

    pub fn text_range(&self) -> TextRange {
        self.idents()
            .iter()
            .fold(self.idents()[0].text_range(), |r, t| {
                r.cover(t.text_range())
            })
    }

    /// Determines whether the key starts with
    /// the same dotted keys as other.
    pub fn is_part_of(&self, other: &KeyNode) -> bool {
        if other.idents().len() < self.idents().len() {
            return false;
        }

        for (a, b) in self.keys_str().zip(other.keys_str()) {
            if a != b {
                return false;
            }
        }

        true
    }

    /// Determines whether the key starts with
    /// the same dotted keys as other.
    pub fn contains(&self, other: &KeyNode) -> bool {
        other.is_part_of(self)
    }

    /// retains n idents from the left,
    /// e.g.: outer.inner => super
    /// there will be at least one ident remaining
    pub fn outer(mut self, n: usize) -> Self {
        self.idents.truncate(usize::max(1, n));
        self
    }

    /// skips n idents from the left,
    /// e.g.: outer.inner => inner
    /// there will be at least one ident remaining
    pub fn inner(mut self, n: usize) -> Self {
        let mut max_end_range = self.idents.len();
        if max_end_range > 0 {
            max_end_range -= 1;
        }

        let max_range = usize::min(n, max_end_range);

        if max_range == 0 {
            return self;
        }

        self.idents.drain(0..max_range);
        self
    }

    /// Counts the shared prefix keys, ignores index
    pub fn common_prefix_count(&self, other: &KeyNode) -> usize {
        let mut count = 0;

        for (a, b) in self.keys_str().zip(other.keys_str()) {
            if a != b {
                break;
            }
            count += 1;
        }

        count
    }

    /// Eq that ignores the index of the key
    pub fn eq_keys(&self, other: &KeyNode) -> bool {
        self.key_count() == other.key_count() && self.is_part_of(other)
    }

    /// Prepends other's idents, and also inherits
    /// other's index.
    fn with_prefix(mut self, other: &KeyNode) -> Self {
        self.idents.splice(0..0, other.idents.clone().into_iter());
        self.index = other.index;
        self
    }

    /// Removes other's prefix from self
    fn without_prefix(self, other: &KeyNode) -> Self {
        let count = self.common_prefix_count(other);

        if count > 0 {
            self.inner(count)
        } else {
            self
        }
    }

    fn with_index(mut self, index: usize) -> Self {
        self.index = index;
        self
    }

    fn prefix(self) -> Self {
        let count = self.key_count();
        self.outer(count - 1)
    }

    fn last(self) -> Self {
        let count = self.key_count();
        self.inner(count)
    }
}

impl PartialEq for KeyNode {
    fn eq(&self, other: &Self) -> bool {
        self.eq_keys(other) && self.index == other.index
    }
}

impl Eq for KeyNode {}

// Needed because of custom PartialEq
impl Hash for KeyNode {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for s in self.keys_str() {
            s.hash(state)
        }
        self.index.hash(state)
    }
}

impl core::fmt::Display for KeyNode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.full_key().fmt(f)
    }
}

impl Cast for KeyNode {
    fn cast(element: SyntaxElement) -> Option<Self> {
        if element.kind() != KEY {
            None
        } else {
            element.into_node().and_then(|n| {
                Some(Self {
                    idents: {
                        let i: Vec<SyntaxToken> = n
                            .children_with_tokens()
                            .filter_map(|c| {
                                if let rowan::NodeOrToken::Token(t) = c {
                                    match t.kind() {
                                        IDENT => Some(t),
                                        _ => None,
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if i.len() == 0 {
                            return None;
                        }
                        i
                    },
                    index: 0,
                    syntax: n,
                })
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueNode {
    Bool(BoolNode),
    String(StringNode),
    Integer(IntegerNode),
    Float(FloatNode),
    Array(ArrayNode),
    Date(DateNode),
    Table(TableNode),

    // Only for convenience purposes during parsing,
    // it is not an actually valid value,
    // and will probably cause panics if used as one.
    Empty,
}

impl Default for ValueNode {
    fn default() -> Self {
        ValueNode::Empty
    }
}

impl ValueNode {
    fn cdom_inner(element: SyntaxElement) -> Option<Self> {
        match element.kind() {
            INLINE_TABLE => Cast::cast(element).map(|v| ValueNode::Table(v)),
            ARRAY => Cast::cast(element).map(|v| ValueNode::Array(v)),
            BOOL => Cast::cast(element).map(|v| ValueNode::Bool(v)),
            STRING | STRING_LITERAL | MULTI_LINE_STRING | MULTI_LINE_STRING_LITERAL => {
                Cast::cast(element).map(|v| ValueNode::String(v))
            }
            INTEGER | INTEGER_BIN | INTEGER_HEX | INTEGER_OCT => {
                Cast::cast(element).map(|v| ValueNode::Integer(v))
            }
            FLOAT => Cast::cast(element).map(|v| ValueNode::Float(v)),
            DATE => Cast::cast(element).map(|v| ValueNode::Date(v)),
            _ => None,
        }
    }

    pub fn text_range(&self) -> TextRange {
        match self {
            ValueNode::Bool(v) => v.text_range(),
            ValueNode::String(v) => v.text_range(),
            ValueNode::Integer(v) => v.text_range(),
            ValueNode::Float(v) => v.text_range(),
            ValueNode::Array(v) => v.text_range(),
            ValueNode::Date(v) => v.text_range(),
            ValueNode::Table(v) => v.text_range(),
            _ => panic!("empty value"),
        }
    }

    pub fn kind(&self) -> SyntaxKind {
        match self {
            ValueNode::Bool(v) => v.kind(),
            ValueNode::String(v) => v.kind(),
            ValueNode::Integer(v) => v.kind(),
            ValueNode::Float(v) => v.kind(),
            ValueNode::Array(v) => v.kind(),
            ValueNode::Date(v) => v.kind(),
            ValueNode::Table(v) => v.kind(),
            _ => panic!("empty value"),
        }
    }
}

impl core::fmt::Display for ValueNode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ValueNode::Bool(v) => v.fmt(f),
            ValueNode::String(v) => v.fmt(f),
            ValueNode::Integer(v) => v.fmt(f),
            ValueNode::Float(v) => v.fmt(f),
            ValueNode::Array(v) => v.fmt(f),
            ValueNode::Date(v) => v.fmt(f),
            ValueNode::Table(v) => v.fmt(f),
            _ => panic!("empty value"),
        }
    }
}

impl Cast for ValueNode {
    fn cast(element: SyntaxElement) -> Option<Self> {
        element
            .into_node()
            .and_then(|n| n.first_child_or_token())
            .and_then(|c| match c.kind() {
                INLINE_TABLE => Cast::cast(c).map(|v| ValueNode::Table(v)),
                ARRAY => Cast::cast(c).map(|v| ValueNode::Array(v)),
                BOOL => Cast::cast(c).map(|v| ValueNode::Bool(v)),
                STRING | STRING_LITERAL | MULTI_LINE_STRING | MULTI_LINE_STRING_LITERAL => {
                    Cast::cast(c).map(|v| ValueNode::String(v))
                }
                INTEGER | INTEGER_BIN | INTEGER_HEX | INTEGER_OCT => {
                    Cast::cast(c).map(|v| ValueNode::Integer(v))
                }
                FLOAT => Cast::cast(c).map(|v| ValueNode::Float(v)),
                DATE => Cast::cast(c).map(|v| ValueNode::Date(v)),
                _ => None,
            })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum IntegerRepr {
    Dec,
    Bin,
    Oct,
    Hex,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IntegerNode {
    syntax: SyntaxToken,
    repr: IntegerRepr,
}

impl IntegerNode {
    pub fn repr(&self) -> IntegerRepr {
        self.repr
    }
}

impl Cast for IntegerNode {
    fn cast(element: SyntaxElement) -> Option<Self> {
        match element.kind() {
            INTEGER => Some(IntegerNode {
                syntax: element.into_token().unwrap(),
                repr: IntegerRepr::Dec,
            }),
            INTEGER_BIN => Some(IntegerNode {
                syntax: element.into_token().unwrap(),
                repr: IntegerRepr::Bin,
            }),
            INTEGER_HEX => Some(IntegerNode {
                syntax: element.into_token().unwrap(),
                repr: IntegerRepr::Hex,
            }),
            INTEGER_OCT => Some(IntegerNode {
                syntax: element.into_token().unwrap(),
                repr: IntegerRepr::Oct,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum StringKind {
    Basic,
    MultiLine,
    Literal,
    MultiLineLiteral,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StringNode {
    syntax: SyntaxToken,
    kind: StringKind,

    /// Escaped and trimmed value.
    content: String,
}

impl StringNode {
    pub fn string_kind(&self) -> StringKind {
        self.kind
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn into_content(self) -> String {
        self.content
    }
}

impl Cast for StringNode {
    fn cast(element: SyntaxElement) -> Option<Self> {
        match element.kind() {
            STRING => Some(StringNode {
                kind: StringKind::Basic,
                content: match unescape(
                    element
                        .as_token()
                        .unwrap()
                        .text()
                        .as_str()
                        .remove_prefix(r#"""#)
                        .remove_suffix(r#"""#),
                ) {
                    Ok(s) => s,
                    Err(_) => return None,
                },
                syntax: element.into_token().unwrap(),
            }),
            MULTI_LINE_STRING => Some(StringNode {
                kind: StringKind::MultiLine,
                content: match unescape(
                    element
                        .as_token()
                        .unwrap()
                        .text()
                        .as_str()
                        .remove_prefix(r#"""""#)
                        .remove_suffix(r#"""""#)
                        .remove_prefix("\n"),
                ) {
                    Ok(s) => s,
                    Err(_) => return None,
                },
                syntax: element.into_token().unwrap(),
            }),
            STRING_LITERAL => Some(StringNode {
                kind: StringKind::Literal,
                content: element
                    .as_token()
                    .unwrap()
                    .text()
                    .as_str()
                    .remove_prefix(r#"'"#)
                    .remove_suffix(r#"'"#)
                    .into(),
                syntax: element.into_token().unwrap(),
            }),
            MULTI_LINE_STRING_LITERAL => Some(StringNode {
                kind: StringKind::MultiLineLiteral,
                content: element
                    .as_token()
                    .unwrap()
                    .text()
                    .as_str()
                    .remove_prefix(r#"'''"#)
                    .remove_suffix(r#"'''"#)
                    .remove_prefix("\n")
                    .into(),
                syntax: element.into_token().unwrap(),
            }),
            _ => None,
        }
    }
}

dom_primitives!(
    BOOL => BoolNode,
    FLOAT => FloatNode,
    DATE => DateNode
);

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum Error {
    DuplicateKey { first: KeyNode, second: KeyNode },
    ExpectedTableArray { target: KeyNode, key: KeyNode },
    ExpectedTable { target: KeyNode, key: KeyNode },
    InlineTable { target: KeyNode, key: KeyNode },
    Spanned { range: TextRange, message: String },
    Generic(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::DuplicateKey { first, second } => write!(
                f,
                "duplicate keys: \"{}\" ({:?}) and \"{}\" ({:?})",
                &first.full_key(),
                &first.text_range(),
                &second.full_key(),
                &second.text_range()
            ),
            Error::ExpectedTable { target, key } => write!(
                f,
                "Expected \"{}\" ({:?}) to be a table, but it is not, required by \"{}\" ({:?})",
                &target.full_key(),
                &target.text_range(),
                &key.full_key(),
                &key.text_range()
            ),
            Error::ExpectedTableArray { target, key } => write!(
                f,
                "\"{}\" ({:?}) conflicts with array of tables: \"{}\" ({:?})",
                &target.full_key(),
                &target.text_range(),
                &key.full_key(),
                &key.text_range()
            ),
            Error::InlineTable { target, key } => write!(
                f,
                "inline tables cannot be modified: \"{}\" ({:?}), modification attempted here: \"{}\" ({:?})",
                &target.full_key(),
                &target.text_range(),
                &key.full_key(),
                &key.text_range()
            ),
            Error::Spanned { range, message } => write!(f, "{} ({:?})", message, range),
            Error::Generic(s) => s.fmt(f),
        }
    }
}
impl std::error::Error for Error {}