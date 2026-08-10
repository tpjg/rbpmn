// Shared DI generation: wires <incoming>/<outgoing> references (the corpus
// omits them; rbpmn derives connectivity from sequence flows), runs
// bpmn-auto-layout, and moves subprocess children onto their own drill-down
// planes. Used by scripts/add-di.mjs (baking DI into fixtures) and by the
// playground at runtime for hand-typed XML without DI.
import { BpmnModdle } from 'bpmn-moddle';
import { layoutProcess } from 'bpmn-auto-layout';

const moddle = new BpmnModdle();

function wireFlows(container) {
  for (const el of container.flowElements ?? []) {
    if (el.$type === 'bpmn:SequenceFlow') {
      if (el.sourceRef) {
        const outgoing = el.sourceRef.get('outgoing');
        if (!outgoing.includes(el)) outgoing.push(el);
      }
      if (el.targetRef) {
        const incoming = el.targetRef.get('incoming');
        if (!incoming.includes(el)) incoming.push(el);
      }
    }
    if (el.$type === 'bpmn:SubProcess') wireFlows(el);
  }
}

async function splitSubprocessPlanes(xml) {
  const { rootElement: definitions } = await moddle.fromXML(xml);
  const subprocesses = [];
  const collect = (container) => {
    for (const el of container.flowElements ?? []) {
      if (el.$type === 'bpmn:SubProcess') {
        subprocesses.push(el);
        collect(el);
      }
    }
  };
  for (const root of definitions.rootElements ?? []) {
    if (root.$type === 'bpmn:Process') collect(root);
  }
  if (!subprocesses.length) return xml;

  const rootPlane = definitions.diagrams[0].plane;
  for (const sp of subprocesses) {
    const memberIds = new Set((sp.flowElements ?? []).map((el) => el.id));
    const moved = rootPlane.planeElement.filter((di) => memberIds.has(di.bpmnElement?.id));
    rootPlane.planeElement = rootPlane.planeElement.filter((di) => !moved.includes(di));
    const plane = moddle.create('bpmndi:BPMNPlane', {
      id: `BPMNPlane_${sp.id}`,
      bpmnElement: sp,
      planeElement: moved,
    });
    const diagram = moddle.create('bpmndi:BPMNDiagram', {
      id: `BPMNDiagram_${sp.id}`,
      plane,
    });
    for (const di of moved) di.$parent = plane;
    plane.$parent = diagram;
    diagram.$parent = definitions;
    definitions.get('diagrams').push(diagram);
  }
  const { xml: out } = await moddle.toXML(definitions, { format: true });
  return out;
}

/// XML in, XML-with-DI out. XML that already carries a diagram is returned
/// unchanged.
export async function ensureDi(xml) {
  if (xml.includes('bpmndi:BPMNDiagram')) return xml;
  const { rootElement: definitions } = await moddle.fromXML(xml);
  for (const root of definitions.rootElements ?? []) {
    if (root.$type === 'bpmn:Process') wireFlows(root);
  }
  const { xml: wired } = await moddle.toXML(definitions, { format: true });
  return splitSubprocessPlanes(await layoutProcess(wired));
}
