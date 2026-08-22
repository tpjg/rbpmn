-- The published read-only surface for definitions: `rbpmn_v_definition`, and
-- its artifacts in `rbpmn_v_definition_decision`.
--
-- The other four views answer what is *happening*. This one answers what is
-- *deployed*: which key is at which version, what its content hash is, when
-- it landed, and — because a definition is not a diagram but a diagram plus
-- its manifest plus the decisions it invokes — the artifacts themselves. An
-- application reconciling "the model in git" against "the model that is
-- running" is asking exactly this, and was reading `rbpmn_definition` to do
-- it.
--
-- Same contract as the rest: columns may be added, never removed or
-- repurposed; plain inlinable projections — no WHERE, no LIMIT, no DISTINCT,
-- no ORDER BY, no aggregate, no volatile function, and NOT `security_barrier`.
--
-- ------------------------------------------------------------ two views
--
-- The artifacts do not all fit in one. `bpmn_xml` and `bindings` are 1:1 with
-- the definition and sit here; the DMN artifacts are 0..N and would need an
-- `array_agg` to fold in, which would stop this being an inlinable projection
-- — the same reason `rbpmn_v_subscription` leaves ambiguity to a query. So
-- they get a projection of their own, keyed by the stable pair as well as the
-- surrogate so a caller can join whichever way it already holds.
--
-- `ordinal` is not decoration: artifacts may import one another, so the order
-- they were deployed in is part of the deployment (0010). Read them ordered.
--
-- --------------------------------------------------------------- caution
--
-- `bpmn_xml` and `dmn_xml` are whole documents. `select *` here pulls every
-- model in the installation across the wire, which is almost never what was
-- meant — name the columns, and reach for the XML only when the answer is the
-- model itself.
--
-- `retired_instances` is exposed because it is the reason
-- `delete_definition` refuses a version that looks unreferenced: retention
-- removes the instance rows, and this counter is what remembers that their
-- archived history still needs this model's element ids to be intelligible.
-- Without it in the surface, that refusal reads as arbitrary.
--
-- --------------------------------------------------------------- indexing
--
-- None added, deliberately. Definitions are bounded by *deploys*, not by
-- throughput — a few versions per process, a few KB each — so there is no
-- scan here worth preventing, and the primary key plus the `(key, version)`
-- unique index already serve both "this exact version" and "the latest of
-- this key" (the unique index scans backwards for free). An index added
-- against a table that does not grow is maintenance for nobody.
create view rbpmn_v_definition as
select
    d.id,
    d.key,
    d.version,
    d.content_hash,
    d.deployed_at,
    d.bpmn_xml,
    d.bindings,
    d.retired_instances
from rbpmn_definition d;

comment on view rbpmn_v_definition is
    'Public read-only projection of deployed definitions, carrying the artifacts that are 1:1 with them (bpmn_xml, bindings). Stable API: columns may be added, never removed or repurposed. Plain inlinable view by design. The DMN artifacts are in rbpmn_v_definition_decision; bpmn_xml is a whole document, so name your columns.';

create view rbpmn_v_definition_decision as
select
    dd.definition_id,
    d.key as definition_key,
    d.version as definition_version,
    dd.ordinal,
    dd.dmn_xml
from rbpmn_definition_decision dd
join rbpmn_definition d on d.id = dd.definition_id;

comment on view rbpmn_v_definition_decision is
    'Public read-only projection of the DMN artifacts a definition was deployed with. Stable API: columns may be added, never removed or repurposed. Plain inlinable view by design. Read ordered by ordinal: artifacts may import one another, so deployment order is part of the deployment.';
