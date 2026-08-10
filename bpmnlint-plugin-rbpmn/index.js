// bpmnlint-plugin-rbpmn: rbpmn's deploy-time rules inside bpmn-io tooling.
//
// Usage (.bpmnlintrc):
//   { "extends": ["bpmnlint:recommended", "plugin:rbpmn/recommended"] }
//
// Rule IDs are rbpmn's stable public API; severities mirror the engine's
// catalogue. `rbpmn/unresolved-topic` is deploy-time only (it checks against
// handler registration state the XML cannot carry) and never fires here —
// it is listed so configs can reference the complete catalogue.
export default {
  configs: {
    recommended: {
      rules: {
        'rbpmn/no-inclusive-gateway': 'error',
        'rbpmn/no-call-activity': 'error',
        'rbpmn/no-unsupported-element': 'error',
        'rbpmn/balanced-gateways': 'error',
        'rbpmn/single-start-event': 'error',
        'rbpmn/conditions-feel-subset': 'error',
        'rbpmn/timer-iso8601': 'error',
        'rbpmn/message-has-correlation': 'error',
        'rbpmn/no-foreign-implementation': 'warn',
        'rbpmn/boundary-on-supported-host': 'error',
        'rbpmn/no-implicit-split': 'error',
        'rbpmn/implicit-merge-after-parallel': 'warn',
        'rbpmn/bpmn-structure': 'error',
        'rbpmn/no-mixed-gateway': 'error',
        'rbpmn/event-gateway-structure': 'error',
        'rbpmn/unresolved-topic': 'error',
      },
    },
  },
};
