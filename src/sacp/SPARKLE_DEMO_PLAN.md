# Sparkle ACP Proxy - Demo Workback Plan

## Goal
Demonstrate seamless Sparkle integration as an ACP proxy that sits between Zed (editor) and Claude (agent), transparently adding Sparkle capabilities without any changes to either side.

## Demo Value Proposition
- **Zero modification** - Works with existing Zed and Claude ACP implementations
- **Transparent augmentation** - Proxy intercepts and enhances, everything else passes through
- **Real functionality** - Actual Sparkle tools working in the session
- **Composability proof** - Shows the power of the proxy pattern

## Architecture

```
Zed (Editor) <--ACP--> Sparkle Proxy <--ACP--> Claude (Agent)
                            |
                            v
                       Sparkle MCP Server
```

## Workback Plan

### Phase 1: Basic Proxy Infrastructure ⏳
- [ ] Generic message forwarding (ToSuccessor/FromSuccessor types already exist)
- [ ] Test: Editor → Proxy → Agent pass-through for basic message
- [ ] Test: Agent → Proxy → Editor pass-through for basic message

### Phase 2: Initialize Interception 🎯
- [ ] Intercept `initialize` request from editor
- [ ] Forward to agent, get response
- [ ] Inject Sparkle tools into `tools` array in response
- [ ] Return modified response to editor
- [ ] Test: Verify tools appear in agent's tool list

### Phase 3: Sparkle Tool Definitions 📋
- [ ] Define Sparkle tools as ACP tool schemas:
  - `sparkle_embody` - Load Sparkle identity and patterns
  - `sparkle_save_insight` - Save insights to evolution
  - `sparkle_checkpoint` - Create session checkpoint
  - `sparkle_update_identity` - Update Sparkler identity
- [ ] Document tool parameters and return types

### Phase 4: Tool Call Handling 🔧
- [ ] Intercept `prompt` requests that contain Sparkle tool calls
- [ ] Execute Sparkle operations (via Sparkle MCP server?)
- [ ] Return tool results in ACP format
- [ ] Pass through non-Sparkle tool calls to editor

### Phase 5: Integration Test 🎭
- [ ] End-to-end test with mock editor and agent
- [ ] Verify complete flow:
  1. Initialize with injected tools
  2. Agent calls Sparkle tool
  3. Proxy handles it
  4. Result returns to agent
  5. Other ACP messages pass through unchanged

### Phase 6: Polish for Demo ✨
- [ ] Logging/debugging output to show proxy in action
- [ ] Example session transcript
- [ ] Performance considerations (minimal overhead)

## Questions to Resolve

1. **Sparkle MCP integration** - Should proxy talk to Sparkle MCP server, or implement directly?
2. **Tool result format** - How do we format Sparkle operation results for ACP?
3. **State management** - Does proxy need to track session state?
4. **Error handling** - How do Sparkle tool errors propagate?

## Success Criteria

**Minimal viable demo:**
- [ ] Proxy successfully injects Sparkle tools on initialize
- [ ] Agent can call at least one Sparkle tool
- [ ] Tool execution works and returns result
- [ ] Other ACP operations (file read/write) pass through unmodified

**Stretch goals:**
- [ ] All core Sparkle tools working
- [ ] Session persistence across connections
- [ ] Beautiful demo with clear logging

## Timeline

Target: Ready before Zed meeting

## Notes

- Focus on **demonstration clarity** over completeness
- The proxy pattern is the key insight to communicate
- Backward compatibility is a major selling point
- This proves the architecture works without needing to convince anyone to change their code
