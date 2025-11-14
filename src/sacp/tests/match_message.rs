use sacp::{
    Component, Handled, JrHandlerChain, JrMessage, JrMessageHandler, JrRequest, JrResponsePayload,
    util::MatchMessage,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct EchoRequestResponse {
    text: Vec<String>,
}

impl JrMessage for EchoRequestResponse {
    fn into_untyped_message(self) -> Result<sacp::UntypedMessage, sacp::Error> {
        Ok(sacp::UntypedMessage {
            method: self.method().to_string(),
            params: sacp::util::json_cast(self)?,
        })
    }

    fn method(&self) -> &str {
        "echo"
    }

    fn parse_request(
        method: &str,
        params: &impl serde::Serialize,
    ) -> Option<Result<Self, sacp::Error>> {
        if method == "echo" {
            Some(sacp::util::json_cast(params))
        } else {
            None
        }
    }

    fn parse_notification(
        _method: &str,
        _params: &impl serde::Serialize,
    ) -> Option<Result<Self, sacp::Error>> {
        None
    }
}

impl JrResponsePayload for EchoRequestResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, sacp::Error> {
        sacp::util::json_cast(self)
    }

    fn from_value(_method: &str, value: serde_json::Value) -> Result<Self, sacp::Error> {
        sacp::util::json_cast(value)
    }
}

impl JrRequest for EchoRequestResponse {
    type Response = EchoRequestResponse;
}

struct EchoHandler;

impl JrMessageHandler for EchoHandler {
    async fn handle_message(
        &mut self,
        message: sacp::MessageAndCx,
    ) -> Result<sacp::Handled<sacp::MessageAndCx>, sacp::Error> {
        MatchMessage::new(message)
            .if_request(async move |request: EchoRequestResponse, request_cx| {
                request_cx.respond(request)
            })
            .await
            .done()
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "TestHandler"
    }
}

#[tokio::test]
async fn modify_message_en_route() -> Result<(), sacp::Error> {
    // Demonstrate a case where we modify a message
    // using a `JrMessageHandler` invoked from `MatchMessage`

    struct TestComponent;

    impl Component for TestComponent {
        async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
            JrHandlerChain::new()
                .with_handler(PushHandler {
                    message: "b".to_string(),
                })
                .with_handler(EchoHandler)
                .serve(client)
                .await
        }
    }

    struct PushHandler {
        message: String,
    }

    impl JrMessageHandler for PushHandler {
        async fn handle_message(
            &mut self,
            message: sacp::MessageAndCx,
        ) -> Result<sacp::Handled<sacp::MessageAndCx>, sacp::Error> {
            MatchMessage::new(message)
                .if_request(async move |mut request: EchoRequestResponse, request_cx| {
                    request.text.push(self.message.clone());
                    Ok(Handled::No((request, request_cx)))
                })
                .await
                .done()
        }

        fn describe_chain(&self) -> impl std::fmt::Debug {
            "TestHandler"
        }
    }

    JrHandlerChain::new()
        .connect_to(TestComponent)?
        .with_client(async |cx| {
            let result = cx
                .send_request(EchoRequestResponse {
                    text: vec!["a".to_string()],
                })
                .block_task()
                .await?;

            expect_test::expect![[r#"
                EchoRequestResponse {
                    text: [
                        "a",
                        "b",
                    ],
                }
            "#]]
            .assert_debug_eq(&result);
            Ok(())
        })
        .await
}

#[tokio::test]
async fn modify_message_en_route_inline() -> Result<(), sacp::Error> {
    // Demonstrate a case where we modify a message en route using an `on_receive_request` call

    struct TestComponent;

    impl Component for TestComponent {
        async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
            JrHandlerChain::new()
                .on_receive_request(async move |mut request: EchoRequestResponse, request_cx| {
                    request.text.push("b".to_string());
                    Ok(Handled::No((request, request_cx)))
                })
                .with_handler(EchoHandler)
                .serve(client)
                .await
        }
    }

    JrHandlerChain::new()
        .connect_to(TestComponent)?
        .with_client(async |cx| {
            let result = cx
                .send_request(EchoRequestResponse {
                    text: vec!["a".to_string()],
                })
                .block_task()
                .await?;

            expect_test::expect![[r#"
                EchoRequestResponse {
                    text: [
                        "a",
                        "b",
                    ],
                }
            "#]]
            .assert_debug_eq(&result);
            Ok(())
        })
        .await
}

#[tokio::test]
async fn modify_message_and_stop() -> Result<(), sacp::Error> {
    // Demonstrate a case where we have an async handler that just returns `()`
    // in front (and hence we never see the `'b`).

    struct TestComponent;

    impl Component for TestComponent {
        async fn serve(self, client: impl Component) -> Result<(), sacp::Error> {
            JrHandlerChain::new()
                .on_receive_request(async move |request: EchoRequestResponse, request_cx| {
                    request_cx.respond(request)
                })
                .on_receive_request(async move |mut request: EchoRequestResponse, request_cx| {
                    request.text.push("b".to_string());
                    Ok(Handled::No((request, request_cx)))
                })
                .with_handler(EchoHandler)
                .serve(client)
                .await
        }
    }

    JrHandlerChain::new()
        .connect_to(TestComponent)?
        .with_client(async |cx| {
            let result = cx
                .send_request(EchoRequestResponse {
                    text: vec!["a".to_string()],
                })
                .block_task()
                .await?;

            expect_test::expect![[r#"
                EchoRequestResponse {
                    text: [
                        "a",
                    ],
                }
            "#]]
            .assert_debug_eq(&result);
            Ok(())
        })
        .await
}
