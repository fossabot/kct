pub mod extension;
pub mod input;
pub mod property;
pub mod release;
mod resolvers;

use self::extension::Extension;
pub use self::input::Input;
use self::property::Property;
pub use self::release::Release;
use self::resolvers::*;

use crate::error::{Error, Result};

use derive_builder::Builder;
use jrsonnet_evaluator::FuncVal;
use jrsonnet_evaluator::{
	error::Error as JrError,
	error::LocError,
	trace::{ExplainingFormat, PathResolver},
	EvaluationState, ManifestFormat, Val,
};
use serde_json::Value;
use std::collections::HashMap;
use std::convert::From;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const LIB_CODE: &str = include_str!("lib.libsonnet");
pub const VARS_PREFIX: &str = "kct.io";

#[derive(Clone, Default, Builder)]
#[builder(pattern = "owned")]
pub struct Workspace {
	pub root: PathBuf,
	pub entrypoint: PathBuf,
	pub lib: PathBuf,
	pub vendor: Rc<PathBuf>,
}

impl Workspace {
	pub(crate) fn setup(&self, builder: WorkspaceBuilder) -> WorkspaceBuilder {
		let builder = match builder.vendor {
			None => builder.vendor(Rc::clone(&self.vendor)),
			Some(_) => builder,
		};

		let builder = match builder.lib {
			None => builder.lib(self.lib.clone()),
			Some(_) => builder,
		};

		let builder = match builder.root {
			None => builder.root(self.root.clone()),
			Some(_) => builder,
		};

		match builder.entrypoint {
			None => builder.entrypoint(self.entrypoint.clone()),
			Some(_) => builder,
		}
	}
}

pub trait Validator: Fn(&Compiler) -> bool {}
impl<T: Fn(&Compiler) -> bool> Validator for T {}

#[derive(Clone, Default)]
pub struct Compiler {
	pub(crate) workspace: Workspace,
	properties: HashMap<property::Name, Rc<Box<dyn Property>>>,
	extensions: HashMap<extension::Name, Rc<Box<dyn Extension>>>,
	validators: Vec<Rc<Box<dyn Validator>>>,
}

impl TryFrom<WorkspaceBuilder> for Compiler {
	type Error = Error;

	fn try_from(builder: WorkspaceBuilder) -> Result<Self> {
		Compiler::setup(builder)
			.build()
			.map_err(|_| Error::InvalidInput)
			.map(|workspace| Compiler {
				workspace,
				..Default::default()
			})
	}
}

impl Compiler {
	pub fn workspace(mut self, workspace: Workspace) -> Self {
		self.workspace = workspace;

		self
	}

	pub fn prop(mut self, prop: Box<dyn Property>) -> Self {
		self.properties.insert(prop.name(), Rc::new(prop));

		self
	}

	pub fn extension(mut self, ext: Box<dyn Extension>) -> Self {
		self.extensions.insert(ext.name(), Rc::new(ext));

		self
	}

	pub fn validator<F: 'static + Validator>(mut self, validator: F) -> Self {
		self.validators.push(Rc::new(Box::new(validator)));

		self
	}

	pub fn compile(self) -> Result<Value> {
		let render_issue = |err: LocError| {
			let message = match err.error() {
				JrError::ImportSyntaxError { path, .. } => {
					format!("syntax error at {}", path.display())
				}
				err => err.to_string(),
			};

			Error::RenderIssue(message)
		};

		for validator in self.validators.iter() {
			if !validator(&self) {
				return Err(Error::InvalidInput);
			}
		}

		let state = self.create_state();

		let variables = self.create_ext_vars();
		for (name, value) in variables {
			let name = format!("{}/{}", VARS_PREFIX, name);
			state.add_ext_var(name.into(), value);
		}

		let parsed = state
			.evaluate_file_raw(&self.workspace.entrypoint)
			.map_err(render_issue)?;

		let rendered = state.manifest(parsed).map_err(render_issue)?.to_string();

		let json = serde_json::from_str(&rendered).map_err(|_err| Error::InvalidOutput)?;

		Ok(json)
	}

	fn create_ext_vars(&self) -> HashMap<String, Val> {
		let from_prop = |p: property::Name| -> (String, Val) {
			let default = Val::Null;
			let name = p.as_str();
			let property = self.properties.get(&p);

			let val = property
				.map(|value| {
					let val = value.generate();

					Val::from(&val)
				})
				.unwrap_or(default);

			(String::from(name), val)
		};

		let from_ext = |e: extension::Name| -> (String, Val) {
			let name = e.as_str();
			let val = match self.extensions.get(&e) {
				None => Val::Null,
				Some(ext) => {
					let func = ext.generate(self);
					let ext: Rc<FuncVal> = Rc::new(FuncVal::NativeExt(name.into(), Rc::new(func)));

					Val::Func(ext)
				}
			};

			(String::from(name), val)
		};

		vec![
			from_prop(property::Name::Package),
			from_prop(property::Name::Release),
			from_prop(property::Name::Input),
			from_ext(extension::Name::Include),
			from_ext(extension::Name::File),
		]
		.into_iter()
		.collect()
	}

	fn create_state(&self) -> EvaluationState {
		let state = EvaluationState::default();
		let resolver = PathResolver::Absolute;
		state.set_trace_format(Box::new(ExplainingFormat { resolver }));

		state.with_stdlib();

		let vendor = self.workspace.vendor.to_path_buf();
		let lib = self.workspace.lib.clone();

		let sdk_resolver = Box::new(StaticImportResolver {
			path: PathBuf::from(VARS_PREFIX),
			contents: String::from(LIB_CODE),
		});

		let relative_resolver = Box::new(RelativeImportResolver);

		let lib_resolver = Box::new(LibImportResolver {
			library_paths: vec![vendor, lib],
		});

		let resolver = AggregatedImportResolver::default()
			.push(sdk_resolver)
			.push(relative_resolver)
			.push(lib_resolver);

		state.set_import_resolver(Box::new(resolver));

		state.set_manifest_format(ManifestFormat::Json(0));

		state
	}
}

impl Compiler {
	fn setup(builder: WorkspaceBuilder) -> WorkspaceBuilder {
		match builder.root {
			None => builder,
			Some(ref root) => {
				let lib = Self::default_lib(root);
				let vendor = Rc::new(Self::default_vendor(root));

				let builder = match builder.lib {
					None => builder.lib(lib),
					Some(_) => builder,
				};

				match builder.vendor {
					None => builder.vendor(vendor),
					Some(_) => builder,
				}
			}
		}
	}

	fn default_vendor(root: &Path) -> PathBuf {
		let mut path = root.to_path_buf();
		path.push("vendor");

		path
	}

	fn default_lib(root: &Path) -> PathBuf {
		let mut path = root.to_path_buf();
		path.push("lib");

		path
	}
}

#[derive(Clone)]
pub struct Compilation {
	pub package: Option<Rc<Value>>,
	pub input: Option<Rc<Value>>,
	pub release: Option<Rc<Value>>,
}

impl From<&Compiler> for Compilation {
	fn from(compiler: &Compiler) -> Self {
		let get_prop = |p: property::Name| -> Option<Rc<Value>> {
			compiler
				.properties
				.get(&p)
				.map(|v| v.generate())
				.map(Rc::new)
		};

		let package = get_prop(property::Name::Package);
		let input = get_prop(property::Name::Input);
		let release = get_prop(property::Name::Release);

		Compilation {
			package,
			input,
			release,
		}
	}
}
