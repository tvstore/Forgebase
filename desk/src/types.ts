// Shapes of the JSON the API returns (see docs/blueprint/architecture.md, section 7).

export type Access = 'all' | 'own' | 'none';

export type Right = 'read' | 'write' | 'create' | 'delete' | 'submit' | 'cancel' | 'amend';

export type Rights = Record<Right, Access>;

export interface SessionInfo {
  username: string;
  roles: string[];
  csrf_token: string;
}

export interface DocTypeSummary {
  name: string;
  label: string;
  /** The app that owns the DocType, or null when none is recorded. */
  app: string | null;
  submittable: boolean;
  rights: Rights;
}

export type FieldType =
  | 'data'
  | 'text'
  | 'int'
  | 'decimal'
  | 'check'
  | 'date'
  | 'datetime'
  | 'select'
  | 'link'
  | 'table';

export interface FieldMeta {
  name: string;
  label: string;
  type: FieldType;
  required: boolean;
  unique: boolean;
  allow_on_submit: boolean;
  /** Written only by the server (hooks): shown read-only and never sent. */
  read_only: boolean;
  max_length?: number;
  precision?: number;
  scale?: number;
  options?: string[];
  target?: string;
  /** Table fields only: the fields of the child DocType. */
  fields?: FieldMeta[];
}

export interface DocTypeMeta {
  name: string;
  label: string;
  app: string | null;
  naming: string;
  submittable: boolean;
  /** The field that names a document for a person, or null. */
  title_field: string | null;
  /** The columns of the list after `name`, or null for the default. */
  list_fields: string[] | null;
  fields: FieldMeta[];
  rights: Rights;
}

/** A listed document: the standard columns plus one key per field. */
export type DocumentRow = Record<string, unknown>;
