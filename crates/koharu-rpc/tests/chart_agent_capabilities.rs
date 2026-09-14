#[test]
fn chart_agent_capability_route_is_registered() {
    let (_, openapi) = koharu_rpc::api();
    assert!(
        openapi
            .paths
            .paths
            .contains_key("/chart-agent/capabilities"),
        "Chart Agent capability endpoint must be part of the public API"
    );
}
