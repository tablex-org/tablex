//! Read-only view of the schema metadata that is *already cached*. The
//! completion core never loads anything: an implementation answers from
//! memory or reports "not loaded".

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Relation<'a> {
    /// Schema / database of the relation; `None` when it has no namespace
    /// (treated as the default one).
    pub namespace: Option<&'a str>,
    pub name: &'a str,
    pub view: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Column<'a> {
    pub name: &'a str,
    pub data_type: Option<&'a str>,
}

pub trait Catalog {
    /// Tables and views visible to the editor.
    fn relations<'a>(&'a self, visit: &mut dyn FnMut(Relation<'a>));

    /// Schemas / databases (the default one included).
    fn namespaces<'a>(&'a self, visit: &mut dyn FnMut(&'a str));

    /// Namespace that unqualified names resolve to.
    fn default_namespace(&self) -> Option<&str> {
        None
    }

    /// Columns of a relation. Returns `false` when they are not cached.
    fn columns<'a>(
        &'a self,
        namespace: Option<&str>,
        relation: &str,
        visit: &mut dyn FnMut(Column<'a>),
    ) -> bool;

    /// Mongo collection names of the editor's database.
    fn collections<'a>(&'a self, visit: &mut dyn FnMut(&'a str));

    /// Field names observed in the editor's collection (current result columns).
    fn fields<'a>(&'a self, visit: &mut dyn FnMut(&'a str));
}

/// A catalog with nothing cached.
#[cfg(test)]
pub struct EmptyCatalog;

#[cfg(test)]
impl Catalog for EmptyCatalog {
    fn relations<'a>(&'a self, _: &mut dyn FnMut(Relation<'a>)) {}
    fn namespaces<'a>(&'a self, _: &mut dyn FnMut(&'a str)) {}
    fn columns<'a>(&'a self, _: Option<&str>, _: &str, _: &mut dyn FnMut(Column<'a>)) -> bool {
        false
    }
    fn collections<'a>(&'a self, _: &mut dyn FnMut(&'a str)) {}
    fn fields<'a>(&'a self, _: &mut dyn FnMut(&'a str)) {}
}

/// `(namespace, table, [(column, type)])`.
#[cfg(test)]
pub type TableColumns = (Option<String>, String, Vec<(String, Option<String>)>);

/// In-memory catalog for tests and benchmarks.
#[cfg(test)]
#[derive(Default)]
pub struct StaticCatalog {
    pub relations: Vec<(Option<String>, String, bool)>,
    pub namespaces: Vec<String>,
    pub columns: Vec<TableColumns>,
    pub collections: Vec<String>,
    pub fields: Vec<String>,
    pub default_namespace: Option<String>,
}

#[cfg(test)]
impl StaticCatalog {
    pub fn table(mut self, name: &str, columns: &[&str]) -> Self {
        self.relations.push((None, name.to_owned(), false));
        self.columns.push((
            None,
            name.to_owned(),
            columns
                .iter()
                .map(|column| ((*column).to_owned(), None))
                .collect(),
        ));
        self
    }

    pub fn typed_table(mut self, name: &str, columns: &[(&str, &str)]) -> Self {
        self.relations.push((None, name.to_owned(), false));
        self.columns.push((
            None,
            name.to_owned(),
            columns
                .iter()
                .map(|(column, ty)| ((*column).to_owned(), Some((*ty).to_owned())))
                .collect(),
        ));
        self
    }

    pub fn view(mut self, name: &str) -> Self {
        self.relations.push((None, name.to_owned(), true));
        self
    }

    pub fn in_schema(mut self, schema: &str, table: &str, columns: &[&str]) -> Self {
        if !self.namespaces.iter().any(|known| known == schema) {
            self.namespaces.push(schema.to_owned());
        }
        self.relations
            .push((Some(schema.to_owned()), table.to_owned(), false));
        self.columns.push((
            Some(schema.to_owned()),
            table.to_owned(),
            columns
                .iter()
                .map(|column| ((*column).to_owned(), None))
                .collect(),
        ));
        self
    }

    pub fn collections(mut self, names: &[&str]) -> Self {
        self.collections = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    pub fn fields(mut self, names: &[&str]) -> Self {
        self.fields = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }
}

#[cfg(test)]
impl Catalog for StaticCatalog {
    fn default_namespace(&self) -> Option<&str> {
        self.default_namespace.as_deref()
    }

    fn relations<'a>(&'a self, visit: &mut dyn FnMut(Relation<'a>)) {
        for (namespace, name, view) in &self.relations {
            visit(Relation {
                namespace: namespace.as_deref(),
                name,
                view: *view,
            });
        }
    }

    fn namespaces<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        self.namespaces
            .iter()
            .for_each(|namespace| visit(namespace));
    }

    fn columns<'a>(
        &'a self,
        namespace: Option<&str>,
        relation: &str,
        visit: &mut dyn FnMut(Column<'a>),
    ) -> bool {
        let found = self.columns.iter().find(|(ns, name, _)| {
            ns.as_deref() == namespace && name.eq_ignore_ascii_case(relation)
        });
        let Some((_, _, columns)) = found else {
            return false;
        };
        for (name, data_type) in columns {
            visit(Column {
                name,
                data_type: data_type.as_deref(),
            });
        }
        true
    }

    fn collections<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        self.collections.iter().for_each(|name| visit(name));
    }

    fn fields<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        self.fields.iter().for_each(|name| visit(name));
    }
}
