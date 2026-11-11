// Jig IR v1. Mirrors ir.schema.json; catalog.json lists the primitives and props.
// The VAM app vendors a copy pinned by the digest of spec/jig/.

export type Literal = string | number | boolean | null | Literal[] | { [key: string]: Literal };

/** A value read from the jig's input data (a JSON pointer, e.g. "/question"). */
export interface DataBinding { $data: string }
/** A value read from a state variable. */
export interface StateBinding { $state: string }
/** A field of the current element inside a List (a JSON pointer into the array's items, e.g. "/subject"). Only valid below a List. */
export interface ItemBinding { $item: string }
export type Binding = DataBinding | StateBinding | ItemBinding;

/** A named user event. Button presses raise one. */
export type Action =
  | { event: "submit"; with?: string[]; set?: { [state: string]: Literal } }
  | { event: "set"; state: string; value: Literal };

export interface JigNode {
  /** A primitive name from catalog.json. */
  type: string;
  /** Each prop is a literal or a binding; "action" props hold an Action. */
  props?: { [prop: string]: Literal | Binding | Action };
  /** Child node ids, for primitives that take children (Stack). */
  children?: string[];
  /** Shown only when the bound value is non-empty. */
  visible?: Binding;
  /** Shown only when the bound value is empty. */
  hidden?: Binding;
}

export interface JigIR {
  version: 1;
  /** Catalog name and version, e.g. "vam/1". */
  catalog: string;
  /** Jig name, e.g. "vam.question". */
  jig: string;
  root: string;
  nodes: { [id: string]: JigNode };
  /** State variables and their defaults. */
  state?: { [name: string]: Literal };
  /** JSON Schema (draft-07 subset) of the data bound in. */
  inputSchema?: object;
  /** JSON Schema (draft-07 subset) of the submit result. */
  resultSchema?: object;
}
