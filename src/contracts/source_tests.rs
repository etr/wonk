use super::*;
use crate::contracts::source::extract_test_helpers::*;

// -- per-kind options (TASK-087, PRD-CTR-REQ-001) --------------------------

#[test]
fn contract_options_default_enables_all_kinds() {
    let opts = ContractOptions::default();
    assert!(opts.enabled(ContractKind::Http));
    assert!(opts.enabled(ContractKind::Env));
    assert!(opts.enabled(ContractKind::Queue));
    assert!(opts.enabled(ContractKind::WebSocket));
    assert!(opts.enabled(ContractKind::Job));
    assert!(opts.enabled(ContractKind::Grpc));
    assert!(opts.enabled(ContractKind::Graphql));
    assert!(opts.enabled(ContractKind::Openapi));
}

#[test]
fn contract_options_from_config_maps_every_flag() {
    let cfg = crate::config::ContractsConfig {
        http: true,
        env: false,
        queue: false,
        websocket: true,
        job: false,
        grpc: false,
        graphql: true,
        openapi: false,
        workspace: Vec::new(),
    };
    let opts = ContractOptions::from(&cfg);
    assert!(opts.enabled(ContractKind::Http));
    assert!(!opts.enabled(ContractKind::Env));
    assert!(!opts.enabled(ContractKind::Queue));
    assert!(opts.enabled(ContractKind::WebSocket));
    assert!(!opts.enabled(ContractKind::Job));
    assert!(!opts.enabled(ContractKind::Grpc));
    assert!(opts.enabled(ContractKind::Graphql));
    assert!(!opts.enabled(ContractKind::Openapi));
}

#[test]
fn contract_options_is_copy() {
    let opts = ContractOptions::default();
    let copy = opts;
    assert_eq!(
        copy.enabled(ContractKind::Queue),
        opts.enabled(ContractKind::Queue)
    );
}

#[test]
fn extract_with_all_kinds_disabled_returns_empty() {
    let src = "const app = express();\napp.get('/v1/users/:id', h);\nconst db = process.env.DATABASE_URL;\n";
    let opts = ContractOptions {
        http: false,
        env: false,
        queue: false,
        websocket: false,
        job: false,
        grpc: false,
        graphql: false,
        openapi: false,
    };
    assert!(extract_with(Lang::JavaScript, src, &opts).is_empty());
}

#[test]
fn extract_with_http_disabled_keeps_env() {
    let src = "const app = express();\napp.get('/v1/users/:id', h);\nconst db = process.env.DATABASE_URL;\n";
    let opts = ContractOptions {
        http: false,
        ..ContractOptions::default()
    };
    let cands = extract_with(Lang::JavaScript, src, &opts);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].kind, ContractKind::Env);
}

#[test]
fn extract_with_env_disabled_keeps_http() {
    let src = "const app = express();\napp.get('/v1/users/:id', h);\nconst db = process.env.DATABASE_URL;\n";
    let opts = ContractOptions {
        env: false,
        ..ContractOptions::default()
    };
    let cands = extract_with(Lang::JavaScript, src, &opts);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].kind, ContractKind::Http);
}

// -- walker: JavaScript / TypeScript (step 4) -------------------------------

#[test]
fn express_provider() {
    let src = "const app = express();\napp.get('/v1/users/:id', handler);\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.kind, ContractKind::Http);
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
    assert_eq!(
        c.params,
        vec![PathParam {
            position: 1,
            name: "id".into()
        }]
    );
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.line, 2);
    assert_eq!(c.owning_symbol, None);
}

#[test]
fn express_router_var_binding() {
    let src = "const r = express.Router();\nr.post('/orders', createOrder);\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "http::POST::/orders").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn fetch_consumer_absolute_url() {
    let src = "async function load() {\n  const r = await fetch('https://api.io/v1/users');\n}\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "http::GET::/v1/users").expect("route not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("load"));
}

#[test]
fn express_template_consumer() {
    let src = "async function load() {\n  await fetch(`${API_URL}/v1/tags/${id}`);\n}\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "http::GET::/v1/tags/{p1}").expect("route not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(
        c.params,
        vec![PathParam {
            position: 1,
            name: "id".into()
        }]
    );
}

#[test]
fn axios_member_consumer() {
    let src = "const d = await axios.get('/v1/users');\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "http::GET::/v1/users").expect("route not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn process_env_member_read() {
    let src = "const url = process.env.DATABASE_URL;\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "env::::DATABASE_URL").expect("env not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol, None);
}

#[test]
fn process_env_subscript_read() {
    let src = "const k = process.env['API_KEY'];\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "env::::API_KEY").expect("env not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn import_meta_env_read() {
    let src = "const k = import.meta.env.VITE_API_KEY;\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "env::::VITE_API_KEY").expect("env not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn process_env_write_is_ambiguous_provider() {
    let src = "process.env.FEATURE_FLAG = 'on';\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "env::::FEATURE_FLAG");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

// -- walker: queue, JavaScript (TASK-087 step 5) ---------------------------

#[test]
fn kafkajs_producer_send_topic_prop() {
    let src = "const producer = kafka.producer();\nawait producer.send({ topic: 'orders.created', messages: [m] });\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.kind, ContractKind::Queue);
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.canonical_id, "queue::kafka::orders.created");
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.line, 2);
}

#[test]
fn kafkajs_consumer_subscribe_topic_prop() {
    let src = "await consumer.subscribe({ topic: 'orders.created', fromBeginning: true });\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
    assert_eq!(c.kind, ContractKind::Queue);
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn amqplib_send_to_queue() {
    let src = "ch.sendToQueue('orders.created', Buffer.from(msg));\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn amqplib_publish_routing_key() {
    let src = "ch.publish('orders', 'orders.created', Buffer.from(msg));\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::rabbitmq::orders.created");
    assert_eq!(cands[0].role, ContractRole::Consumer);
}

#[test]
fn amqplib_consume() {
    let src = "ch.consume('orders.created', (msg) => {});\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn nats_js_publish() {
    let src = "nc.publish('orders.created', payload);\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn nats_js_subscribe() {
    let src = "nc.subscribe('orders.created', cb);\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn js_generic_send_heuristic() {
    let src = "svc.send('orders.created');\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "queue::::orders.created");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn js_generic_publish_broker_from_receiver() {
    let src = "rabbitChan.publish('orders.created');\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::rabbitmq::orders.created");
    assert_eq!(cands[0].confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn js_generic_subscribe_heuristic() {
    let src = "consumer.subscribe('orders.created');\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "queue::::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn js_socket_send_is_reserved_for_websocket() {
    // ws.send belongs to the websocket kind, never to queue.
    let cands = extract(Lang::JavaScript, "socket.send('hello');");
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].kind, ContractKind::WebSocket);
    assert_eq!(cands[0].canonical_id, "websocket::::hello");
    assert_eq!(cands[0].role, ContractRole::Provider);
}

#[test]
fn js_socket_send_non_literal_skipped() {
    let cands = extract(Lang::JavaScript, "ws.send(JSON.stringify(data));");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn js_queue_non_literal_topic_skipped() {
    let cands = extract(Lang::JavaScript, "svc.send(topic);");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn js_queue_interpolated_topic_skipped() {
    let cands = extract(Lang::JavaScript, "svc.send(`orders.${id}`);");
    assert!(cands.is_empty(), "got {cands:?}");
}

// -- walker: websocket, JS/TS (TASK-087 step 6) ----------------------------

#[test]
fn js_io_emit_is_ws_provider() {
    let src = "io.emit('chat.message', payload);\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.kind, ContractKind::WebSocket);
    assert_eq!(c.canonical_id, "websocket::::chat.message");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn js_socket_emit_is_ws_provider() {
    let cands = extract(Lang::JavaScript, "socket.emit('chat.message', data);");
    let c = find(&cands, "websocket::::chat.message").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn js_socket_broadcast_emit_is_ws_provider() {
    let cands = extract(
        Lang::JavaScript,
        "socket.broadcast.emit('chat.message', data);",
    );
    let c = find(&cands, "websocket::::chat.message").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn js_io_to_room_emit_is_ws_provider() {
    let cands = extract(Lang::JavaScript, "io.to(room).emit('chat.message', d);");
    let c = find(&cands, "websocket::::chat.message").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn js_socket_on_is_ws_consumer() {
    let src = "socket.on('chat.message', (msg) => {});\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "websocket::::chat.message");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn js_ws_on_requires_handler_argument() {
    // `.on` with a bare event name and no handler is not a registration.
    let cands = extract(Lang::JavaScript, "socket.on('chat.message');");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn js_generic_emit_heuristic() {
    let cands = extract(Lang::JavaScript, "events.emit('user.created', data);");
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "websocket::::user.created");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn js_generic_on_is_skipped() {
    // EventEmitter `.on` registrations flood every codebase — skipped.
    let cands = extract(Lang::JavaScript, "emitter.on('tick', cb);");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn js_server_on_listening_skipped() {
    // Node http idiom: server.on('listening', cb) is a lifecycle hook,
    // not a websocket registration — falls through to the generic `.on`
    // skip like any other EventEmitter.
    let cands = extract(Lang::JavaScript, "server.on('listening', cb);");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn js_conn_on_data_skipped() {
    // Node net idiom: conn.on('data', cb) is a plain stream read, not a
    // websocket registration — falls through to the generic `.on` skip.
    let cands = extract(Lang::JavaScript, "conn.on('data', (chunk) => {});");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn js_ws_receiver_whitelist_stays_websocket() {
    // Every whitelisted receiver name emits websocket contracts.
    for recv in ["io", "socket", "ws", "wss", "websocket"] {
        let cands = extract(
            Lang::JavaScript,
            &format!("{recv}.emit('chat.message', d);"),
        );
        assert_eq!(cands.len(), 1, "{recv}: got {cands:?}");
        assert_eq!(cands[0].kind, ContractKind::WebSocket, "{recv}");
        assert_eq!(cands[0].canonical_id, "websocket::::chat.message", "{recv}");
        assert_eq!(cands[0].confidence, CONFIDENCE_FRAMEWORK, "{recv}");
    }
}

#[test]
fn js_express_ws_route_is_ws_consumer() {
    let src = "const app = express();\napp.ws('/chat', handler);\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "websocket::::/chat");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

// -- walker: Python (step 5) ------------------------------------------------

#[test]
fn flask_decorator_provider() {
    let src = "\
from flask import Flask
app = Flask(__name__)

@app.get('/v1/users/<int:id>')
def get_user(id):
    return {}
";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
    assert_eq!(
        c.params,
        vec![PathParam {
            position: 1,
            name: "id".into()
        }]
    );
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("get_user"));
    assert_eq!(c.line, 4);
}

#[test]
fn flask_route_methods_kwarg_sets_verb() {
    let src = "\
app = Flask(__name__)

@app.route('/orders', methods=['POST'])
def create_order():
    return {}
";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "http::POST::/orders").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("create_order"));
}

#[test]
fn flask_route_without_methods_is_get() {
    let src = "\
@app.route('/health')
def health():
    return {}
";
    let cands = extract(Lang::Python, src);
    assert!(
        find(&cands, "http::GET::/health").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn requests_consumer_absolute_url() {
    let src = "\
def load():
    r = requests.get('https://api.io/v1/users')
";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "http::GET::/v1/users").expect("route not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("load"));
}

#[test]
fn httpx_and_session_consumers() {
    let src = "\
def load():
    a = httpx.get('/v1/users')
    b = session.get('/v1/users')
    c = client.get('/v1/users')
    d = urlopen('/health')
";
    let cands = extract(Lang::Python, src);
    for id in ["http::GET::/v1/users", "http::GET::/health"] {
        assert!(find(&cands, id).is_some(), "missing {id}: {cands:?}");
    }
    assert_eq!(cands.len(), 4, "got {cands:?}");
    assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
}

#[test]
fn python_fstring_consumer() {
    let src = "\
def load():
    r = requests.get(f'{BASE_URL}/users/{user_id}')
";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "http::GET::/users/{p1}").expect("route not found");
    assert_eq!(
        c.params,
        vec![PathParam {
            position: 1,
            name: "user_id".into()
        }]
    );
}

#[test]
fn python_env_accessors() {
    let src = "\
def cfg():
    a = os.environ['DATABASE_URL']
    b = os.environ.get('FEATURE_FLAG')
    c = os.getenv('HOME')
";
    let cands = extract(Lang::Python, src);
    for name in ["DATABASE_URL", "FEATURE_FLAG", "HOME"] {
        let c = find(&cands, &format!("env::::{name}")).expect("env not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }
    assert_eq!(cands.len(), 3, "got {cands:?}");
}

#[test]
fn python_env_setdefault_is_ambiguous_provider() {
    let src = "os.environ.setdefault('CACHE_DIR', '/tmp')\n";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "env::::CACHE_DIR");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn python_environ_assign_is_ambiguous_provider() {
    let src = "os.environ['TMP_SET'] = '1'\n";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "env::::TMP_SET");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn django_urlpatterns_providers() {
    let src = "\
from django.urls import path
import views

urlpatterns = [
    path('users/<int:id>', views.user_detail),
    path('health', views.health),
]
";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "http::ANY::/users/{p1}").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(
        c.params,
        vec![PathParam {
            position: 1,
            name: "id".into()
        }]
    );
    assert!(find(&cands, "http::ANY::/health").is_some());
    assert_eq!(cands.len(), 2, "got {cands:?}");
}

#[test]
fn falcon_add_route_provider() {
    let src = "api.add_route('/things', ThingsResource())\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "http::ANY::/things").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

// -- walker: queue, Python (TASK-087 step 5) -------------------------------

#[test]
fn py_kafka_producer_send_heuristic() {
    let src = "def run():\n    producer.send('orders.created', value=msg)\n";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "queue::::orders.created");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    assert_eq!(c.owning_symbol.as_deref(), Some("run"));
}

#[test]
fn py_confluent_producer_produce() {
    let src = "producer.produce('orders.created', value=msg)\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn py_kafka_consumer_subscribe_list() {
    let src = "consumer.subscribe(['orders.created'])\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn py_consumer_subscribe_plain_string_heuristic() {
    let src = "consumer.subscribe('orders.created')\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn py_consumer_subscribe_multi_topic_list_skipped() {
    let cands = extract(
        Lang::Python,
        "consumer.subscribe(['a.created', 'b.created'])",
    );
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn py_nats_publish() {
    let src = "async def push():\n    await nc.publish('orders.created', b'x')\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn py_nats_subscribe() {
    let src = "async def listen():\n    await nc.subscribe('orders.created')\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn py_pika_basic_publish_kwarg() {
    let src = "ch.basic_publish(exchange='', routing_key='orders.created', body=msg)\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn py_pika_basic_publish_positional() {
    let src = "ch.basic_publish('', 'orders.created', msg)\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn py_pika_basic_consume_kwarg() {
    let src = "ch.basic_consume(queue='orders.created', on_message_callback=cb)\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn py_pika_basic_consume_positional() {
    let src = "ch.basic_consume('orders.created', cb)\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn py_generic_send_broker_from_receiver() {
    let src = "kafka_producer.send('orders.created')\n";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::kafka::orders.created");
    assert_eq!(cands[0].confidence, CONFIDENCE_HEURISTIC);
}

// -- walker: Ruby (step 6) ---------------------------------------------------

#[test]
fn ruby_sinatra_provider() {
    let src = "\
get '/v1/users/:id' do
  json
end
";
    let cands = extract(Lang::Ruby, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(
        c.params,
        vec![PathParam {
            position: 1,
            name: "id".into()
        }]
    );
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn ruby_rails_match_is_any() {
    let src = "match '/health', to: 'health#show', via: :all\n";
    let cands = extract(Lang::Ruby, src);
    let c = find(&cands, "http::ANY::/health").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn ruby_consumers() {
    let src = "\
def pull
  HTTParty.get('https://api.io/v1/users')
  RestClient.get('/v1/users')
  Faraday.get('/v1/users')
end
";
    let cands = extract(Lang::Ruby, src);
    assert_eq!(cands.len(), 3, "got {cands:?}");
    assert!(
        cands
            .iter()
            .all(|c| c.canonical_id == "http::GET::/v1/users")
    );
    assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
    assert!(
        cands
            .iter()
            .all(|c| c.owning_symbol.as_deref() == Some("pull"))
    );
}

#[test]
fn ruby_env() {
    let src = "\
db = ENV['DATABASE_URL']
k = ENV.fetch('KEY')
ENV['TMP_SET'] = 'x'
";
    let cands = extract(Lang::Ruby, src);
    assert_eq!(cands.len(), 3, "got {cands:?}");
    let db = find(&cands, "env::::DATABASE_URL").expect("db not found");
    assert_eq!(db.role, ContractRole::Consumer);
    assert!(find(&cands, "env::::KEY").is_some());
    let w = find(&cands, "env::::TMP_SET").expect("write not found");
    assert_eq!(w.role, ContractRole::Provider);
    assert_eq!(w.confidence, CONFIDENCE_HEURISTIC);
}

// -- walker: queue, Ruby (TASK-087 step 5) ---------------------------------

#[test]
fn ruby_bunny_publish_routing_key_kwarg() {
    let src = "x.publish(payload, routing_key: 'orders.created')\n";
    let cands = extract(Lang::Ruby, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "queue::rabbitmq::orders.created");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn ruby_bunny_queue_subscribe_binding() {
    let src = "q = channel.queue('orders.created')\nq.subscribe do |info, props, body|\n  puts body\nend\n";
    let cands = extract(Lang::Ruby, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "queue::rabbitmq::orders.created");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn ruby_subscribe_on_unbound_var_skipped() {
    // No channel.queue binding: nothing to attribute the topic from.
    let cands = extract(Lang::Ruby, "q.subscribe do |info, body|\nend\n");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn ruby_generic_publish_heuristic() {
    let src = "chan.publish('orders.created')\n";
    let cands = extract(Lang::Ruby, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::::orders.created");
    assert_eq!(cands[0].role, ContractRole::Consumer);
    assert_eq!(cands[0].confidence, CONFIDENCE_HEURISTIC);
}

// -- walker: Go (step 6) -----------------------------------------------------

#[test]
fn go_gin_provider() {
    let src = "\
func main() {
	r := gin.New()
	r.GET(\"/users/:id\", getUser)
	r.POST(\"/orders\", createOrder)
}
";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let c = find(&cands, "http::GET::/users/{p1}").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(
        c.params,
        vec![PathParam {
            position: 1,
            name: "id".into()
        }]
    );
    assert_eq!(c.owning_symbol.as_deref(), Some("main"));
    assert!(find(&cands, "http::POST::/orders").is_some());
}

#[test]
fn go_handlefunc_is_any() {
    let src = "\
func main() {
	mux.HandleFunc(\"/health\", health)
	http.Handle(\"/static/\", files)
}
";
    let cands = extract(Lang::Go, src);
    assert!(
        find(&cands, "http::ANY::/health").is_some(),
        "got {cands:?}"
    );
    assert!(
        find(&cands, "http::ANY::/static").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn go_consumers() {
    let src = "\
func call() {
	resp, _ := http.Get(\"https://api.io/v1/users\")
	req, _ := http.NewRequest(\"POST\", \"/v1/orders\", nil)
	c, _ := client.Get(\"/v1/users\")
}
";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 3, "got {cands:?}");
    assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
    assert!(find(&cands, "http::GET::/v1/users").is_some());
    assert!(find(&cands, "http::POST::/v1/orders").is_some());
}

#[test]
fn go_env() {
    let src = "\
func cfg() {
	k := os.Getenv(\"DATABASE_URL\")
	l, ok := os.LookupEnv(\"FLAG\")
	os.Setenv(\"TMP_SET\", \"x\")
}
";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 3, "got {cands:?}");
    assert!(find(&cands, "env::::DATABASE_URL").is_some());
    assert!(find(&cands, "env::::FLAG").is_some());
    let w = find(&cands, "env::::TMP_SET").expect("write not found");
    assert_eq!(w.role, ContractRole::Provider);
}

// -- walker: queue, Go (TASK-087 step 5) -----------------------------------

#[test]
fn go_nats_publish_two_args() {
    let src = "package main\n\nfunc push() {\n\tnat.Publish(\"orders.created\", data)\n}\n";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "queue::nats::orders.created");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.line, 4);
}

#[test]
fn go_amqp_publish_three_args_is_rabbitmq() {
    let src = "package main\n\nfunc pub() {\n\tch.Publish(\"orders\", \"orders.created\", false, false, msg)\n}\n";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::rabbitmq::orders.created");
    assert_eq!(cands[0].role, ContractRole::Consumer);
}

#[test]
fn go_amqp_publish_with_context_six_args_is_rabbitmq() {
    let src = "package main\n\nfunc pub() {\n\tch.PublishWithContext(ctx, \"orders\", \"orders.created\", false, false, msg)\n}\n";
    let cands = extract(Lang::Go, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn go_publish_with_context_three_args_is_nats() {
    // nats.go: PublishWithContext(ctx, subj, data) — exactly 3 args,
    // subject at position 1. The literal payload is never the topic.
    let src = "package main\n\nfunc pub() {\n\tnc.PublishWithContext(ctx, \"orders.created\", \"body\")\n}\n";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "queue::nats::orders.created");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert!(
        find(&cands, "queue::rabbitmq::body").is_none(),
        "literal data must not be read as the topic: {cands:?}"
    );
}

#[test]
fn go_publish_with_context_three_args_variable_payload_is_nats() {
    let src =
        "package main\n\nfunc pub() {\n\tnc.PublishWithContext(ctx, \"orders.created\", msg)\n}\n";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::nats::orders.created");
    assert_eq!(cands[0].role, ContractRole::Consumer);
}

#[test]
fn go_publish_with_context_four_plus_args_is_rabbitmq() {
    // amqp091: PublishWithContext(ctx, exchange, key, msg, ...) — the
    // ctx shifts the routing key to position 2.
    let src = "package main\n\nfunc pub() {\n\tch.PublishWithContext(ctx, \"orders\", \"orders.created\", msg)\n}\n";
    let cands = extract(Lang::Go, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn go_publish_four_args_rabbitmq_topic_position() {
    // Plain amqp Publish(exchange, key, ...): topic is arg 1, never the
    // exchange name at arg 0.
    let src = "package main\n\nfunc pub() {\n\tch.Publish(\"orders\", \"orders.created\", false, msg)\n}\n";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::rabbitmq::orders.created");
    assert!(
        find(&cands, "queue::rabbitmq::orders").is_none(),
        "exchange name must not be the topic: {cands:?}"
    );
    assert_eq!(cands[0].role, ContractRole::Consumer);
}

#[test]
fn go_nats_subscribe() {
    let src = "package main\n\nfunc listen() {\n\tnc.Subscribe(\"orders.created\", cb)\n}\n";
    let cands = extract(Lang::Go, src);
    let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn go_nats_queue_subscribe() {
    let src = "package main\n\nfunc listen() {\n\tnc.QueueSubscribe(\"orders.created\", \"grp\", cb)\n}\n";
    let cands = extract(Lang::Go, src);
    let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn go_amqp_consume() {
    let src = "package main\n\nfunc listen() {\n\tmsgs, _ := ch.Consume(\"orders.created\", \"\", true, false, false, false, nil)\n\t_ = msgs\n}\n";
    let cands = extract(Lang::Go, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn go_sarama_consume_partition() {
    let src = "package main\n\nfunc listen() {\n\tpc, _ := consumer.ConsumePartition(\"orders.created\", 0, 0)\n\t_ = pc\n}\n";
    let cands = extract(Lang::Go, src);
    let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn go_sarama_producer_send_message() {
    let src = "package main\n\nfunc pub() {\n\tproducer.SendMessage(&ProducerMessage{Topic: \"orders.created\"})\n}\n";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::kafka::orders.created");
    assert_eq!(cands[0].role, ContractRole::Consumer);
}

#[test]
fn go_publish_arg_count_disambiguates_broker() {
    // 2-positional Publish is nats; 3+ is amqp (paired disambiguation).
    let two = extract(
        Lang::Go,
        "package main\nfunc a() {\n\tnc.Publish(\"s.a\", d)\n}\n",
    );
    let three = extract(
        Lang::Go,
        "package main\nfunc b() {\n\tch.Publish(\"e\", \"s.a\", d)\n}\n",
    );
    assert!(find(&two, "queue::nats::s.a").is_some(), "got {two:?}");
    assert!(
        find(&three, "queue::rabbitmq::s.a").is_some(),
        "got {three:?}"
    );
}

// -- walker: Rust (step 6) ---------------------------------------------------

#[test]
fn rust_attribute_provider() {
    let src = "\
#[get(\"/v1/users/{id}\")]
async fn get_user() -> impl Responder {
    todo!()
}
";
    let cands = extract(Lang::Rust, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(
        c.params,
        vec![PathParam {
            position: 1,
            name: "id".into()
        }]
    );
    assert_eq!(c.owning_symbol.as_deref(), Some("get_user"));
}

#[test]
fn rust_route_attribute_reads_method_kwarg() {
    let src = "\
#[route(\"/v1/orders\", method = \"GET\")]
async fn list_orders() -> impl Responder {
    todo!()
}
";
    let cands = extract(Lang::Rust, src);
    let c = find(&cands, "http::GET::/v1/orders").expect("route not found");
    assert_eq!(c.owning_symbol.as_deref(), Some("list_orders"));
}

#[test]
fn rust_route_call_provider() {
    let src = "\
async fn app() {
    let app = Router::new().route(\"/users/{id}\", get(get_user));
}
";
    let cands = extract(Lang::Rust, src);
    let c = find(&cands, "http::GET::/users/{p1}").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn rust_consumers() {
    let src = "\
async fn calls() {
    let b = reqwest::get(\"https://api.io/v1/users\").await;
    let c = client.get(\"/v1/users\").send().await;
}
";
    let cands = extract(Lang::Rust, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
    assert!(
        cands
            .iter()
            .all(|c| c.canonical_id == "http::GET::/v1/users")
    );
}

#[test]
fn rust_env() {
    let src = "\
fn cfg() {
    let u = std::env::var(\"DATABASE_URL\").unwrap();
    let e = env!(\"API_KEY\");
    std::env::set_var(\"TMP_SET\", \"x\");
}
";
    let cands = extract(Lang::Rust, src);
    assert_eq!(cands.len(), 3, "got {cands:?}");
    assert!(find(&cands, "env::::DATABASE_URL").is_some());
    assert!(find(&cands, "env::::API_KEY").is_some());
    let w = find(&cands, "env::::TMP_SET").expect("write not found");
    assert_eq!(w.role, ContractRole::Provider);
}

// -- walker: queue, Rust (TASK-087 step 5) ---------------------------------

#[test]
fn rust_rdkafka_future_record_to() {
    let src = "fn publish() {\n    let rec = FutureRecord::to(\"orders.created\", 0, payload);\n    producer.send(rec, Timeout::Never);\n}\n";
    let cands = extract(Lang::Rust, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "queue::kafka::orders.created");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.line, 2);
    assert_eq!(c.owning_symbol.as_deref(), Some("publish"));
}

#[test]
fn rust_rdkafka_base_record_to() {
    let src = "fn publish() {\n    let rec = BaseRecord::to(\"orders.created\");\n}\n";
    let cands = extract(Lang::Rust, src);
    let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn rust_rdkafka_consumer_subscribe_slice() {
    let src = "fn listen() {\n    consumer.subscribe(&[\"orders.created\"])?;\n}\n";
    let cands = extract(Lang::Rust, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "queue::kafka::orders.created");
    assert_eq!(cands[0].role, ContractRole::Provider);
    assert_eq!(cands[0].confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn rust_rdkafka_subscribe_multi_topic_slice_skipped() {
    let cands = extract(
        Lang::Rust,
        "fn listen() {\n    consumer.subscribe(&[\"a\", \"b\"])?;\n}\n",
    );
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn rust_async_nats_publish_into() {
    let src =
        "async fn push() {\n    client.publish(\"orders.created\".into(), bytes).await?;\n}\n";
    let cands = extract(Lang::Rust, src);
    let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn rust_async_nats_subscribe_into() {
    let src = "async fn listen() {\n    client.subscribe(\"orders.created\".into()).await?;\n}\n";
    let cands = extract(Lang::Rust, src);
    let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn rust_subscribe_non_string_receiver_skipped() {
    let cands = extract(Lang::Rust, "fn f() {\n    bus.subscribe(handler);\n}\n");
    assert!(cands.is_empty(), "got {cands:?}");
}

// -- walker: Java (step 6) ---------------------------------------------------

#[test]
fn java_spring_provider() {
    let src = "\
public class UserController {

    @GetMapping(\"/users/{id}\")
    public String getUser(@PathVariable String id) { return \"\"; }

    @PostMapping(\"/orders\")
    public String create() { return \"\"; }
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let c = find(&cands, "http::GET::/users/{p1}").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("getUser"));
    assert!(find(&cands, "http::POST::/orders").is_some());
}

#[test]
fn java_jaxrs_provider() {
    let src = "\
@Path(\"/items\")
public class ItemsResource {

    @GET
    @Path(\"/{id}\")
    public String item() { return \"\"; }
}
";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "http::GET::/items/{p1}").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("item"));
}

#[test]
fn java_consumers() {
    let src = "\
class Client {
    String call() {
        String r = restTemplate.getForObject(\"https://api.io/v1/users\", String.class);
        String p = restTemplate.postForObject(\"/v1/orders\", req, String.class);
        return r;
    }
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(find(&cands, "http::GET::/v1/users").is_some());
    assert!(find(&cands, "http::POST::/v1/orders").is_some());
    assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
}

#[test]
fn java_generic_verbs_need_a_client_receiver() {
    // TASK-082 review debt: `cache.put`/`repository.delete` are not
    // HTTP consumers at full confidence — the generic verbs fire only
    // on an HTTP-looking receiver, matching every other language's
    // allowlist.
    let src = "\
class A {
    void f() {
        cache.put(\"/users/1\", u);
        repository.delete(\"/users/1\");
        map.execute(\"/things\");
    }
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 0, "got {cands:?}");
}

#[test]
fn java_webclient_generic_verbs_are_consumers() {
    let src = "\
class A {
    void f() {
        webClient.put(\"/users/1\");
        restTemplate.delete(\"/users/1\");
    }
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(find(&cands, "http::PUT::/users/1").is_some());
    assert!(find(&cands, "http::DELETE::/users/1").is_some());
    assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
}

#[test]
fn java_env() {
    let src = "\
class Client {
    String cfg() {
        String e = System.getenv(\"DATABASE_URL\");
        return e;
    }
}
";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "env::::DATABASE_URL").expect("env not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

// -- walker: queue, Java (TASK-087 step 5) ---------------------------------

#[test]
fn java_kafka_listener_topics_string() {
    let src = "\
@Component
class Orders {
    @KafkaListener(topics = \"orders.created\")
    public void handle(String msg) {}
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "queue::kafka::orders.created");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("handle"));
}

#[test]
fn java_kafka_listener_topics_array_emits_per_element() {
    let src = "\
class Orders {
    @KafkaListener(topics = {\"a.created\", \"b.created\"})
    public void handle(String msg) {}
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(find(&cands, "queue::kafka::a.created").is_some());
    assert!(find(&cands, "queue::kafka::b.created").is_some());
}

#[test]
fn java_rabbit_listener_queues() {
    let src = "\
class Orders {
    @RabbitListener(queues = \"orders.created\")
    public void handle(String msg) {}
}
";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn java_kafka_template_send() {
    let src = "void publish() {\n    kafkaTemplate.send(\"orders.created\", key, value);\n}\n";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("publish"));
}

#[test]
fn java_rabbit_template_convert_and_send() {
    let src = "void publish() {\n    rabbitTemplate.convertAndSend(\"ex\", \"orders.created\", payload);\n}\n";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn java_rabbit_convert_and_send_with_literal_payload() {
    // TASK-087 review debt: when the payload is itself a string
    // literal, the routing key is the literal BEFORE it — the old
    // last-leading rule emitted the queue under the payload string.
    let src = "void publish() {\n    rabbitTemplate.convertAndSend(\"orders.created\", \"payload\");\n}\n";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert!(
        find(&cands, "queue::rabbitmq::payload").is_none(),
        "the payload literal must not name the queue: {cands:?}"
    );
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn java_rabbit_template_send() {
    let src = "void publish() {\n    rabbitTemplate.send(\"orders.created\", msg);\n}\n";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

// -- walker: websocket, Java (TASK-087 step 6) ------------------------------

#[test]
fn java_message_mapping_is_ws_consumer() {
    let src = "\
@Controller
class Orders {
    @MessageMapping(\"orders.new\")
    public void handle(String msg) {}
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "websocket::::orders.new");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("handle"));
}

#[test]
fn java_send_to_is_ws_provider() {
    let src = "\
@Controller
class Orders {
    @SendTo(\"/topic/orders\")
    public void handle(String msg) {}
}
";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "websocket::::topic.orders").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn java_messaging_template_convert_and_send_is_ws_provider() {
    let src =
        "void push() {\n    messagingTemplate.convertAndSend(\"/topic/orders\", payload);\n}\n";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "websocket::::topic.orders").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

// -- walker: job (TASK-087 step 7) -----------------------------------------

#[test]
fn py_celery_task_decorator() {
    let src = "@app.task\ndef sync_orders():\n    pass\n";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.kind, ContractKind::Job);
    assert_eq!(c.canonical_id, "job::::sync_orders");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("sync_orders"));
}

#[test]
fn py_celery_task_name_kwarg() {
    let src = "@app.task(name='orders.sync')\ndef sync():\n    pass\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "job::::orders.sync").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("sync"));
}

#[test]
fn py_shared_task_decorator() {
    let src = "@shared_task\ndef sync():\n    pass\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "job::::sync").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn py_celery_delay_consumer() {
    let src = "def enqueue(order):\n    sync_orders.delay(order)\n";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "job::::sync_orders");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("enqueue"));
}

#[test]
fn py_celery_apply_async_consumer() {
    let src = "def enqueue():\n    sync_orders.apply_async(kwargs={'o': 1})\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "job::::sync_orders").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn py_send_task_consumer() {
    let src = "def enqueue():\n    celery_app.send_task('orders.sync', args=[1])\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "job::::orders.sync").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn py_apscheduler_add_job() {
    let src = "def setup():\n    scheduler.add_job(sync_orders, trigger='interval')\n";
    let cands = extract(Lang::Python, src);
    let c = find(&cands, "job::::sync_orders").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn ruby_sidekiq_job_class() {
    let src = "class EmailWorker\n  include Sidekiq::Job\n\n  def perform(id)\n  end\nend\n";
    let cands = extract(Lang::Ruby, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "job::::EmailWorker");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn ruby_sidekiq_worker_module() {
    let src = "class CleanupWorker\n  include Sidekiq::Worker\nend\n";
    let cands = extract(Lang::Ruby, src);
    let c = find(&cands, "job::::CleanupWorker").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn ruby_application_job_superclass() {
    let src = "class NotifyJob < ApplicationJob\n  def perform(user)\n  end\nend\n";
    let cands = extract(Lang::Ruby, src);
    let c = find(&cands, "job::::NotifyJob").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn ruby_sidekiq_perform_async_consumer() {
    let cands = extract(Lang::Ruby, "EmailWorker.perform_async(1, 2)");
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "job::::EmailWorker");
    assert_eq!(cands[0].role, ContractRole::Consumer);
    assert_eq!(cands[0].confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn ruby_active_job_perform_later_consumer() {
    let cands = extract(Lang::Ruby, "NotifyJob.perform_later(user)");
    let c = find(&cands, "job::::NotifyJob").expect("contract not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

#[test]
fn java_scheduled_fixed_rate() {
    let src = "\
class Poller {
    @Scheduled(fixedRate = 5000)
    public void pollOrders() {}
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "job::::pollOrders");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("pollOrders"));
}

#[test]
fn java_scheduled_cron_expression_is_not_identity() {
    let src = "\
class Poller {
    @Scheduled(cron = \"0 0 * * * *\")
    public void cleanup() {}
}
";
    let cands = extract(Lang::Java, src);
    let c = find(&cands, "job::::cleanup").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.line, 3, "job line follows the method name");
}

#[test]
fn js_cron_schedule_callback_name() {
    let src = "cron.schedule('*/5 * * * *', fireTick);\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "job::::fireTick");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn js_cron_schedule_inline_callback_uses_owning() {
    let src = "function poll() {\n  cron.schedule(spec, () => run());\n}\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "job::::poll").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn js_agenda_define() {
    let src = "agenda.define('email-send', handler);\n";
    let cands = extract(Lang::JavaScript, src);
    let c = find(&cands, "job::::email-send").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn js_bullmq_queue_add_heuristic() {
    let src = "emailQueue.add('email-send', data);\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert_eq!(cands[0].canonical_id, "job::::email-send");
    assert_eq!(cands[0].role, ContractRole::Consumer);
    assert_eq!(cands[0].confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn js_cart_add_is_not_a_job() {
    let cands = extract(Lang::JavaScript, "cart.add('item');");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn go_cron_add_func() {
    let src = "package main\n\nfunc setup() {\n\tc.AddFunc(\"*/5 * * * * *\", pollOrders)\n}\n";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "job::::pollOrders");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
}

#[test]
fn go_cron_add_job() {
    let src = "package main\n\nfunc setup() {\n\tc.AddJob(spec, nightlyJob)\n}\n";
    let cands = extract(Lang::Go, src);
    let c = find(&cands, "job::::nightlyJob").expect("contract not found");
    assert_eq!(c.role, ContractRole::Provider);
}

// -- walker: PHP (step 6) ----------------------------------------------------

#[test]
fn php_laravel_provider() {
    let src = "\
<?php
Route::get('/v1/users/{id}', [UserController::class, 'show']);
Route::post('/orders', 'OrderController@store');
";
    let cands = extract(Lang::Php, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let c = find(&cands, "http::GET::/v1/users/{p1}").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert!(find(&cands, "http::POST::/orders").is_some());
}

#[test]
fn php_slim_provider() {
    let src = "\
<?php
$app->get('/slim/x', function ($req, $res) { return $res; });
";
    let cands = extract(Lang::Php, src);
    let c = find(&cands, "http::GET::/slim/x").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn php_symfony_attribute() {
    let src = "\
<?php
class Ctrl {
    #[Route('/sym/x', methods: ['GET'])]
    public function show(): void {}
}
";
    let cands = extract(Lang::Php, src);
    let c = find(&cands, "http::GET::/sym/x").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("show"));
}

#[test]
fn php_consumers() {
    let src = "\
<?php
function load() {
    $r = Http::get('https://api.io/v1/users');
    $c = $client->get('/v1/users');
}
";
    let cands = extract(Lang::Php, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
    assert!(
        cands
            .iter()
            .all(|c| c.canonical_id == "http::GET::/v1/users")
    );
}

#[test]
fn php_env() {
    let src = "\
<?php
function load() {
    $k = $_ENV['DATABASE_URL'];
    putenv('TMP_SET=x');
}
";
    let cands = extract(Lang::Php, src);
    let k = find(&cands, "env::::DATABASE_URL").expect("env not found");
    assert_eq!(k.role, ContractRole::Consumer);
    let w = find(&cands, "env::::TMP_SET").expect("write not found");
    assert_eq!(w.role, ContractRole::Provider);
    assert_eq!(w.confidence, CONFIDENCE_HEURISTIC);
}

// -- walker: C# (step 6) -----------------------------------------------------

#[test]
fn csharp_attribute_provider() {
    let src = "\
public class UsersController : ControllerBase
{
    [HttpGet(\"/v1/users/{id}\")]
    public string GetUser(string id) { return \"\"; }
}
";
    let cands = extract(Lang::CSharp, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
    assert_eq!(c.owning_symbol.as_deref(), Some("GetUser"));
}

#[test]
fn csharp_route_attribute_is_any() {
    let src = "\
public class UsersController
{
    [Route(\"health\")]
    public string Health() { return \"\"; }
}
";
    let cands = extract(Lang::CSharp, src);
    assert!(
        find(&cands, "http::ANY::/health").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn csharp_mapget_provider() {
    let src = "\
class Program {
    static void Map() {
        app.MapGet(\"/mapped/x\", () => \"ok\");
        app.MapPost(\"/mapped/y\", () => \"ok\");
    }
}
";
    let cands = extract(Lang::CSharp, src);
    assert!(
        find(&cands, "http::GET::/mapped/x").is_some(),
        "got {cands:?}"
    );
    assert!(
        find(&cands, "http::POST::/mapped/y").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn csharp_consumers() {
    let src = "\
class Client {
    async Task Load() {
        var r = await httpClient.GetAsync(\"/v1/users\");
        var j = await httpClient.GetFromJsonAsync<string>(\"/v1/users\");
        var m = new HttpRequestMessage(HttpMethod.Get, \"/v1/users\");
    }
}
";
    let cands = extract(Lang::CSharp, src);
    assert_eq!(cands.len(), 3, "got {cands:?}");
    assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
    assert!(
        cands
            .iter()
            .all(|c| c.canonical_id == "http::GET::/v1/users")
    );
}

#[test]
fn csharp_env() {
    let src = "\
class Client {
    void Cfg() {
        var e = Environment.GetEnvironmentVariable(\"DATABASE_URL\");
    }
}
";
    let cands = extract(Lang::CSharp, src);
    let c = find(&cands, "env::::DATABASE_URL").expect("env not found");
    assert_eq!(c.role, ContractRole::Consumer);
}

// -- walker: C / C++ (step 6) ------------------------------------------------

#[test]
fn c_curl_consumer() {
    let src = "\
void fetch_it(void) {
    CURL *h = curl_easy_init();
    curl_easy_setopt(h, CURLOPT_URL, \"https://api.io/v1/users\");
}
";
    let cands = extract(Lang::C, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/v1/users");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.owning_symbol.as_deref(), Some("fetch_it"));
}

#[test]
fn c_env() {
    let src = "\
void cfg(void) {
    char *k = getenv(\"DATABASE_URL\");
    putenv(\"TMP_SET=x\");
}
";
    let cands = extract(Lang::C, src);
    let k = find(&cands, "env::::DATABASE_URL").expect("env not found");
    assert_eq!(k.role, ContractRole::Consumer);
    let w = find(&cands, "env::::TMP_SET").expect("write not found");
    assert_eq!(w.role, ContractRole::Provider);
    assert_eq!(w.confidence, CONFIDENCE_HEURISTIC);
}

// -- ambiguity rules: 0.5 heuristic + path-like noise gate (step 7) -------

#[test]
fn single_letter_router_var_needs_a_path_like_route() {
    // TASK-082 review debt: `JS_ROUTER_VARS` includes `r`, so a redis
    // client bound to `r` used to emit `http::GET::user:1` providers
    // at full confidence — the router arm now carries the same
    // path-likeness gate as the ambiguous arm.
    let src =
        "const r = require('redis').createClient();\nr.get('user:1', cb);\nr.set('k', 'v');\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 0, "got {cands:?}");
    // A genuine router named `r` with a path-like route still emits.
    let src = "const r = express.Router();\nr.get('/orders', h);\n";
    let cands = extract(Lang::JavaScript, src);
    assert!(
        find(&cands, "http::GET::/orders").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn cache_like_receiver_is_not_a_contract() {
    let src = "function load() {
  const u = cache.get('user:1');
  const k = db.get('key');
  const t = cache.get(`${prefix}/key`);
}
";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 0, "got {cands:?}");
}

#[test]
fn unknown_receiver_path_like_is_provider_at_05() {
    let src = "const x = registry.get('/users');\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/users");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn concat_single_literal_is_05() {
    let src = "const r = await fetch('/api/users' + id);\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/api/users");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn concat_multiple_literals_skipped() {
    let src = "const r = await fetch('/api/' + id + '/users');\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 0, "got {cands:?}");
}

#[test]
fn concat_adversarial_depth_is_bounded_not_crashing() {
    // TASK-087 review debt: a left-nested ~10k-term concat chain
    // (bundled/minified JS) used to overflow the thread stack one
    // frame per binary-expression level. The capped walk must return
    // no candidates, not crash, and never act on a partial leaf set.
    let mut src = String::from("const r = await fetch(");
    src.push_str("'/users'");
    for i in 0..10_000 {
        src.push_str(&format!(" + seg{i}"));
    }
    src.push_str(");\n");
    let cands = extract(Lang::JavaScript, &src);
    assert_eq!(cands.len(), 0, "got {cands:?}");
}

#[test]
fn python_unknown_receiver_ambiguity() {
    let src = "def load():
    a = store.get('/items')
    b = store.get('user:1')
";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/items");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn go_capitalized_verb_single_arg_is_05() {
    let src = "func load() {
	x := cache.Get(\"/items\")
	_ = x
}
";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/items");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn php_unknown_object_ambiguity() {
    let src = "<?php
function load() {
    $x = $store->get('/items');
}
";
    let cands = extract(Lang::Php, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/items");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

#[test]
fn ruby_unknown_receiver_ambiguity() {
    let src = "x = svc.get('/items')\n";
    let cands = extract(Lang::Ruby, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "http::GET::/items");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
}

// -- prefix composition (REQ-023, step 8) -----------------------------------

#[test]
fn gin_group_prefix() {
    let src = r#"func main() {
	r := gin.New()
	v1 := r.Group("/v1")
	v1.GET("/users/:id", getUser)
}
"#;
    let cands = extract(Lang::Go, src);
    let c = find(&cands, "http::GET::/v1/users/{p1}").expect("route not found");
    assert_eq!(c.role, ContractRole::Provider);
}

#[test]
fn nested_group_two_levels() {
    let src = r#"func main() {
	r := gin.New()
	api := r.Group("/api")
	v1 := api.Group("/v1")
	v1.GET("/users/:id", getUser)
}
"#;
    let cands = extract(Lang::Go, src);
    assert!(
        find(&cands, "http::GET::/api/v1/users/{p1}").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn express_router_mount_same_file() {
    let src = "const app = express();
const router = express.Router();
app.use('/v1', router);
router.get('/users/:id', getUser);
";
    let cands = extract(Lang::JavaScript, src);
    assert!(
        find(&cands, "http::GET::/v1/users/{p1}").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn express_router_mounted_twice_serves_each_prefix() {
    // TASK-082 review debt: a router mounted under several prefixes
    // serves EACH — two contracts — not the concatenation
    // (/v1/v2/users was the old, wrong output).
    let src = "const app = express();
const router = express.Router();
app.use('/v1', router);
app.use('/v2', router);
router.get('/users', h);
";
    let cands = extract(Lang::JavaScript, src);
    assert!(
        find(&cands, "http::GET::/v1/users").is_some(),
        "got {cands:?}"
    );
    assert!(
        find(&cands, "http::GET::/v2/users").is_some(),
        "got {cands:?}"
    );
    assert!(
        find(&cands, "http::GET::/v1/v2/users").is_none(),
        "concatenated mount leaked: {cands:?}"
    );
}

#[test]
fn flask_blueprint_prefix() {
    let src = "bp = Blueprint('auth', __name__, url_prefix='/auth')

@bp.route('/login', methods=['POST'])
def login():
    return {}

app.register_blueprint(bp, url_prefix='/v1')
";
    let cands = extract(Lang::Python, src);
    assert!(
        find(&cands, "http::POST::/v1/auth/login").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn fastapi_includerouter_prefix() {
    let src = "router = APIRouter(prefix='/users')

@router.get('/{id}')
def get_user(id):
    return {}

app.include_router(router, prefix='/v1')
";
    let cands = extract(Lang::Python, src);
    assert!(
        find(&cands, "http::GET::/v1/users/{p1}").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn actix_scope_chain() {
    let src = r#"async fn app() {
    App::new().service(web::scope("/v1").route("/users/{id}", web::get().to(get_user)));
}
"#;
    let cands = extract(Lang::Rust, src);
    assert!(
        find(&cands, "http::GET::/v1/users/{p1}").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn actix_scope_binding() {
    let src = r#"async fn app() {
    let api = web::scope("/api");
    App::new().service(api.route("/users/{id}", web::get().to(get_user)));
}
"#;
    let cands = extract(Lang::Rust, src);
    assert!(
        find(&cands, "http::GET::/api/users/{p1}").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn axum_nest() {
    let src = r#"async fn app() {
    let user_routes = Router::new().route("/users/{id}", get(get_user));
    let app = Router::new().nest("/v1", user_routes);
}
"#;
    let cands = extract(Lang::Rust, src);
    assert!(
        find(&cands, "http::GET::/v1/users/{p1}").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn spring_classlevel_requestmapping() {
    let src = r#"@RestController
@RequestMapping("/v1")
public class UserController {

    @GetMapping("/users/{id}")
    public String getUser(@PathVariable String id) { return ""; }
}
"#;
    let cands = extract(Lang::Java, src);
    assert!(
        find(&cands, "http::GET::/v1/users/{p1}").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn rails_namespace_block() {
    let src = "Rails.application.routes.draw do
  namespace :v1 do
    get '/users', to: 'users#index'
  end
end
";
    let cands = extract(Lang::Ruby, src);
    assert!(
        find(&cands, "http::GET::/v1/users").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn sinatra_namespace_block() {
    let src = "namespace '/v1' do
  get '/users' do
    json
  end
end
";
    let cands = extract(Lang::Ruby, src);
    assert!(
        find(&cands, "http::GET::/v1/users").is_some(),
        "got {cands:?}"
    );
}

#[test]
fn aspnet_controller_route_attribute() {
    let src = r#"[Route("api/[controller]")]
public class UsersController : ControllerBase
{
    [HttpGet("{id}")]
    public string GetUser(string id) { return ""; }
}
"#;
    let cands = extract(Lang::CSharp, src);
    let c = find(&cands, "http::GET::/api/{p1}/{p2}").expect("route not found");
    assert_eq!(
        c.params,
        vec![
            PathParam {
                position: 1,
                name: "controller".into()
            },
            PathParam {
                position: 2,
                name: "id".into()
            }
        ]
    );
}
// -- cross-framework corpus (AR-017, AR-029, step 10) ----------------------

struct CorpusEntry {
    lang: Lang,
    source: &'static str,
    role: ContractRole,
    confidence: f64,
    canonical_id: &'static str,
    params: &'static [(&'static str, &'static str)],
}

const CORPUS: &[CorpusEntry] = &[
    // Providers of GET /v1/users/{id} across frameworks.
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "const app = express(); app.get('/v1/users/:id', h);",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Python,
        source: "@app.get('/v1/users/<int:id>')\ndef f(id):\n    return {}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Python,
        source: "@app.get('/v1/users/{id}')\ndef f(id):\n    return {}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Go,
        source: "func main() {\n\tr.GET(\"/v1/users/:id\", h)\n}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Rust,
        source: "#[get(\"/v1/users/{id}\")]\nasync fn f() -> impl Responder { todo!() }",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Rust,
        source: "fn app() { let a = Router::new().route(\"/v1/users/{id}\", get(h)); }",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Java,
        source: "class C {\n    @GetMapping(\"/v1/users/{id}\")\n    public String f() { return \"\"; }\n}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Java,
        source: "@Path(\"/v1/users\")\npublic class UsersResource {\n    @GET\n    @Path(\"/{id}\")\n    public String f() { return \"\"; }\n}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Ruby,
        source: "get '/v1/users/:id' do\n  json\nend",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::Php,
        source: "<?php\nRoute::get('/v1/users/{id}', 'UserController@show');",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    CorpusEntry {
        lang: Lang::CSharp,
        source: "public class C {\n    [HttpGet(\"/v1/users/{id}\")]\n    public string F() { return \"\"; }\n}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users/{p1}",
        params: &[("p1", "id")],
    },
    // Consumers of GET /v1/users across languages.
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "async function f() { await fetch('https://api.io/v1/users'); }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "async function f() { await fetch(`${API_URL}/v1/users`); }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "const d = axios.get('/v1/users');",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Python,
        source: "def f():\n    r = requests.get('https://api.io/v1/users')",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Python,
        source: "def f():\n    r = httpx.get('/v1/users')",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Go,
        source: "func f() { r, _ := http.Get(\"https://api.io/v1/users\") }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Rust,
        source: "async fn f() { let r = reqwest::get(\"https://api.io/v1/users\").await; }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Java,
        source: "class C { String f() { return restTemplate.getForObject(\"https://api.io/v1/users\", String.class); } }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Ruby,
        source: "def f\n  HTTParty.get('https://api.io/v1/users')\nend",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Php,
        source: "<?php\nfunction f() { $r = Http::get('https://api.io/v1/users'); }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::CSharp,
        source: "class C { async Task F() { var r = await httpClient.GetAsync(\"/v1/users\"); } }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::C,
        source: "void f(void) { curl_easy_setopt(h, CURLOPT_URL, \"https://api.io/v1/users\"); }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/v1/users",
        params: &[],
    },
    // Leading-parameter route: provider and consumer pair on
    // GET /{tenant}/users across languages.
    CorpusEntry {
        lang: Lang::Java,
        source: "class C {\n    @GetMapping(\"/{tenant}/users\")\n    public String f() { return \"\"; }\n}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/{p1}/users",
        params: &[("p1", "tenant")],
    },
    CorpusEntry {
        lang: Lang::Python,
        source: "def f(tenant):\n    r = httpx.get(f\"/{tenant}/users\")",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "http::GET::/{p1}/users",
        params: &[("p1", "tenant")],
    },
    // Env accessors across languages -> one ID.
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "const d = process.env.DATABASE_URL;",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Python,
        source: "d = os.environ['DATABASE_URL']",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Ruby,
        source: "d = ENV['DATABASE_URL']",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Go,
        source: "func f() { d := os.Getenv(\"DATABASE_URL\") }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Rust,
        source: "fn f() { let d = std::env::var(\"DATABASE_URL\").unwrap(); }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Java,
        source: "class C { String f() { return System.getenv(\"DATABASE_URL\"); } }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Php,
        source: "<?php\n$d = $_ENV['DATABASE_URL'];",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::CSharp,
        source: "class C { string F() { return Environment.GetEnvironmentVariable(\"DATABASE_URL\"); } }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::C,
        source: "void f(void) { char *d = getenv(\"DATABASE_URL\"); }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "env::::DATABASE_URL",
        params: &[],
    },
    // -- message kinds (TASK-087) --------------------------------------
    // Kafka orders.created: producer and consumer of one topic ID.
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "producer.send({ topic: 'orders.created', messages: [m] });",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "queue::kafka::orders.created",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Java,
        source: "class C {\n    @KafkaListener(topics = \"orders.created\")\n    public void handle(String m) {}\n}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "queue::kafka::orders.created",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Rust,
        source: "fn f() { consumer.subscribe(&[\"orders.created\"]).unwrap(); }",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "queue::kafka::orders.created",
        params: &[],
    },
    // NATS orders.created.
    CorpusEntry {
        lang: Lang::Go,
        source: "package main\nfunc a() { nc.Publish(\"orders.created\", d) }",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "queue::nats::orders.created",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Go,
        source: "package main\nfunc a() { nc.Subscribe(\"orders.created\", cb) }",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "queue::nats::orders.created",
        params: &[],
    },
    // RabbitMQ orders.created.
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "ch.consume('orders.created', (m) => {});",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "queue::rabbitmq::orders.created",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Python,
        source: "ch.basic_publish(exchange='', routing_key='orders.created', body=b)",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "queue::rabbitmq::orders.created",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Ruby,
        source: "q = channel.queue('orders.created')\nq.subscribe do |i, p, b|\nend",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "queue::rabbitmq::orders.created",
        params: &[],
    },
    // Websocket chat.message: emit and handler share one ID.
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "io.emit('chat.message', payload);",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "websocket::::chat.message",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::JavaScript,
        source: "socket.on('chat.message', (m) => {});",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "websocket::::chat.message",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Java,
        source: "class C {\n    @SendTo(\"/topic/orders\")\n    public void handle(String m) {}\n}",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "websocket::::topic.orders",
        params: &[],
    },
    // Jobs: definition and enqueue share one ID.
    CorpusEntry {
        lang: Lang::Python,
        source: "@app.task(name='orders.sync')\ndef sync():\n    pass",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "job::::orders.sync",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Python,
        source: "def f():\n    celery_app.send_task('orders.sync')",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "job::::orders.sync",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Ruby,
        source: "class EmailWorker\n  include Sidekiq::Job\nend",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "job::::EmailWorker",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Ruby,
        source: "EmailWorker.perform_async(1)",
        role: ContractRole::Consumer,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "job::::EmailWorker",
        params: &[],
    },
    CorpusEntry {
        lang: Lang::Go,
        source: "package main\nfunc setup() { c.AddFunc(\"*/5 * * * * *\", pollOrders) }",
        role: ContractRole::Provider,
        confidence: CONFIDENCE_FRAMEWORK,
        canonical_id: "job::::pollOrders",
        params: &[],
    },
];

#[test]
fn corpus_entries_all_match() {
    for e in CORPUS {
        let cands = extract(e.lang, e.source);
        let c = find(&cands, e.canonical_id)
            .unwrap_or_else(|| panic!("{}: no {} in {cands:?}", e.lang.name(), e.canonical_id));
        assert_eq!(c.role, e.role, "{} {c:?}", e.lang.name());
        assert_eq!(c.confidence, e.confidence, "{} {c:?}", e.lang.name());
        let params: Vec<(String, String)> = c
            .params
            .iter()
            .map(|p| (format!("p{}", p.position), p.name.clone()))
            .collect();
        let want: Vec<(String, String)> = e
            .params
            .iter()
            .map(|(m, n)| (m.to_string(), n.to_string()))
            .collect();
        assert_eq!(params, want, "{} {c:?}", e.lang.name());
    }
}

#[test]
fn corpus_provider_group_shares_one_id() {
    let provider_langs: std::collections::HashSet<Lang> = CORPUS
        .iter()
        .filter(|e| e.role == ContractRole::Provider)
        .map(|e| e.lang)
        .collect();
    assert!(
        provider_langs.len() >= 4,
        "need providers in 4+ languages, got {}",
        provider_langs.len()
    );
    let ids: std::collections::HashSet<&str> = CORPUS
        .iter()
        .filter(|e| e.role == ContractRole::Provider)
        .filter(|e| e.canonical_id.starts_with("http::GET::/v1/users/"))
        .map(|e| e.canonical_id)
        .collect();
    assert_eq!(ids.len(), 1, "provider IDs diverged: {ids:?}");
}

#[test]
fn corpus_consumer_group_shares_one_id() {
    let entries: Vec<&CorpusEntry> = CORPUS
        .iter()
        .filter(|e| e.canonical_id == "http::GET::/v1/users")
        .collect();
    let langs: std::collections::HashSet<Lang> = entries.iter().map(|e| e.lang).collect();
    assert!(
        langs.len() >= 4,
        "need consumers in 4+ languages, got {}",
        langs.len()
    );
    assert!(entries.iter().all(|e| e.role == ContractRole::Consumer));
}

#[test]
fn corpus_pairs_provider_and_consumer_across_languages() {
    // AR-029: same route spelled as provider in one language and consumer
    // in others resolves into the matching ID family.
    assert!(
        CORPUS
            .iter()
            .any(|e| e.canonical_id == "http::GET::/v1/users/{p1}"
                && e.role == ContractRole::Provider)
    );
    assert!(
        CORPUS
            .iter()
            .any(|e| e.canonical_id == "http::GET::/v1/users" && e.role == ContractRole::Consumer)
    );
}

#[test]
fn corpus_env_group_shares_one_id() {
    let ids: std::collections::HashSet<&str> = CORPUS
        .iter()
        .filter(|e| e.canonical_id.starts_with("env::::"))
        .map(|e| e.canonical_id)
        .collect();
    let want: std::collections::HashSet<&str> = ["env::::DATABASE_URL"].into_iter().collect();
    assert_eq!(ids, want);
}

// -- acceptance tests (1:1 with TASK-082 criteria) --------------------------

#[test]
fn acceptance_four_frameworks_same_id() {
    // Express, Flask/FastAPI, gin, Actix — same canonical ID.
    let cases: &[(Lang, &str)] = &[
        (
            Lang::JavaScript,
            "const app = express(); app.get('/v1/users/:id', h);",
        ),
        (
            Lang::Python,
            "@app.get('/v1/users/<int:id>')\ndef f(id):\n    return {}",
        ),
        (Lang::Go, "func main() { r.GET(\"/v1/users/:id\", h) }"),
        (
            Lang::Rust,
            "#[get(\"/v1/users/{id}\")]\nasync fn f() -> impl Responder { todo!() }",
        ),
    ];
    let mut ids = Vec::new();
    for (lang, src) in cases {
        let cands = extract(*lang, src);
        ids.push(
            cands
                .iter()
                .map(|c| c.canonical_id.clone())
                .max_by_key(|id| id.len())
                .expect("at least one contract"),
        );
    }
    assert!(
        ids.iter().all(|id| id == &ids[0]),
        "canonical IDs diverged: {ids:?}"
    );
    assert_eq!(ids[0], "http::GET::/v1/users/{p1}");
}

#[test]
fn acceptance_scheme_authority_matches_relative() {
    let a = extract(
        Lang::JavaScript,
        "async function f() { await fetch('http://api.example.com/v1/users'); }",
    );
    let b = extract(
        Lang::JavaScript,
        "async function f() { await fetch('/v1/users'); }",
    );
    assert_eq!(a[0].canonical_id, b[0].canonical_id);
    assert_eq!(a[0].canonical_id, "http::GET::/v1/users");
}

#[test]
fn acceptance_base_interpolation_matches_colon_param() {
    let a = extract(
        Lang::JavaScript,
        "async function f() { await fetch(`${API_URL}/v1/tags/${id}`); }",
    );
    let b = extract(
        Lang::JavaScript,
        "const app = express(); app.get('/v1/tags/:id', h);",
    );
    assert_eq!(a[0].canonical_id, b[0].canonical_id);
    assert_eq!(a[0].canonical_id, "http::GET::/v1/tags/{p1}");
}

#[test]
fn acceptance_positional_params_retain_names() {
    let a = extract(
        Lang::JavaScript,
        "const app = express(); app.get('/workspaces/{wid}/tags/{id}', h);",
    );
    let b = extract(
        Lang::JavaScript,
        "const app = express(); app.get('/workspaces/{workspaceId}/tags/{id}', h);",
    );
    assert_eq!(a[0].canonical_id, b[0].canonical_id);
    assert_eq!(a[0].canonical_id, "http::GET::/workspaces/{p1}/tags/{p2}");
    assert_eq!(
        a[0].params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        vec!["wid", "id"]
    );
    assert_eq!(
        b[0].params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        vec!["workspaceId", "id"]
    );
}

#[test]
fn acceptance_group_prefix_v1() {
    let cands = extract(
        Lang::Go,
        "func main() {\n\tr := gin.New()\n\tv1 := r.Group(\"/v1\")\n\tv1.GET(\"/users/:id\", h)\n}",
    );
    assert!(
        cands
            .iter()
            .any(|c| c.canonical_id == "http::GET::/v1/users/{p1}"),
        "got {cands:?}"
    );
}

#[test]
fn acceptance_ambiguous_confidence_05() {
    let http = extract(Lang::JavaScript, "const x = registry.get('/users');");
    assert_eq!(http.len(), 1);
    assert_eq!(http[0].confidence, 0.5);
    assert_eq!(http[0].role, ContractRole::Provider);
    let env = extract(Lang::JavaScript, "process.env.FEATURE_FLAG = 'on';");
    assert_eq!(env.len(), 1);
    assert_eq!(env[0].confidence, 0.5);
    assert_eq!(env[0].role, ContractRole::Provider);
}

#[test]
fn acceptance_extraction_runs_during_build() {
    // End-to-end: build_index extracts contracts on the pipeline path.
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/app.js"),
        "const app = express();\napp.get('/v1/users/:id', h);\n",
    )
    .unwrap();
    let config = crate::config::Config::load_with_paths(None, Some(root)).unwrap();
    let stats = crate::pipeline::build_index_with_config(root, true, &config).unwrap();
    assert_eq!(stats.contract_count, 1, "got {stats:?}");
}

// -- acceptance: message kinds (TASK-087) ---------------------------------

#[test]
fn acceptance_producer_consumer_same_topic_id_cross_repo() {
    // Framework pair: kafkajs producer + Spring @KafkaListener consumer
    // resolve to the same canonical ID across repos and languages.
    let producer = extract(
        Lang::JavaScript,
        "await producer.send({ topic: 'orders.created', messages: [m] });",
    );
    let consumer = extract(
        Lang::Java,
        "class C {\n    @KafkaListener(topics = \"orders.created\")\n    public void handle(String m) {}\n}",
    );
    assert_eq!(producer.len(), 1, "got {producer:?}");
    assert_eq!(consumer.len(), 1, "got {consumer:?}");
    assert_eq!(
        producer[0].canonical_id, consumer[0].canonical_id,
        "cross-repo pair diverged"
    );
    assert_eq!(producer[0].canonical_id, "queue::kafka::orders.created");
    assert_eq!(producer[0].role, ContractRole::Consumer);
    assert_eq!(consumer[0].role, ContractRole::Provider);

    // Generic pair: plain-string send/subscribe on unknown receivers —
    // empty qualifier, 0.5 on both sides, same ID.
    let g_prod = extract(Lang::Python, "producer.send('orders.created')");
    let g_cons = extract(Lang::Python, "consumer.subscribe('orders.created')");
    assert_eq!(g_prod.len(), 1, "got {g_prod:?}");
    assert_eq!(g_cons.len(), 1, "got {g_cons:?}");
    assert_eq!(g_prod[0].canonical_id, g_cons[0].canonical_id);
    assert_eq!(g_prod[0].canonical_id, "queue::::orders.created");
    assert_eq!(g_prod[0].confidence, 0.5);
    assert_eq!(g_cons[0].confidence, 0.5);
}

#[test]
fn acceptance_string_literal_topic_confidence_05() {
    // AR-018: framework-shaped constructs score 1.0 even with string
    // literals; only the generic verb+literal tier scores 0.5.
    let framework = extract(
        Lang::JavaScript,
        "ch.sendToQueue('orders.created', Buffer.from(m));",
    );
    assert_eq!(framework[0].confidence, 1.0);
    let heuristic = extract(Lang::JavaScript, "svc.send('orders.created');");
    assert_eq!(heuristic[0].confidence, 0.5);
    // Jobs: the BullMQ add verb is generic, agenda.define is not.
    let agenda = extract(Lang::JavaScript, "agenda.define('email-send', fn);");
    assert_eq!(agenda[0].confidence, 1.0);
    let bullmq = extract(Lang::JavaScript, "emailQueue.add('email-send', d);");
    assert_eq!(bullmq[0].confidence, 0.5);
}

#[test]
fn acceptance_websocket_pairing() {
    let emit = extract(Lang::JavaScript, "io.emit('chat.message', d);");
    let handler = extract(Lang::JavaScript, "socket.on('chat.message', (m) => {});");
    assert_eq!(emit[0].canonical_id, handler[0].canonical_id);
    assert_eq!(emit[0].canonical_id, "websocket::::chat.message");
    assert_eq!(emit[0].role, ContractRole::Provider);
    assert_eq!(handler[0].role, ContractRole::Consumer);
}

#[test]
fn acceptance_job_pairing() {
    let define = extract(Lang::JavaScript, "agenda.define('email-send', fn);");
    let enqueue = extract(Lang::JavaScript, "emailQueue.add('email-send', d);");
    assert_eq!(define[0].canonical_id, enqueue[0].canonical_id);
    assert_eq!(define[0].canonical_id, "job::::email-send");
    assert_eq!(define[0].role, ContractRole::Provider);
    assert_eq!(enqueue[0].role, ContractRole::Consumer);
}

#[test]
fn acceptance_kinds_independently_disableable() {
    // One idiom of every kind in one file; disabling a kind removes
    // exactly its own contracts and leaves the others byte-identical.
    let src = "\
const app = express();
app.get('/v1/users/:id', h);
const db = process.env.DATABASE_URL;
producer.send({ topic: 'orders.created', messages: [m] });
io.emit('chat.message', payload);
agenda.define('email-send', fn);
";
    let baseline = extract(Lang::JavaScript, src);
    assert_eq!(baseline.len(), 5, "got {baseline:?}");
    let mut kinds: Vec<ContractKind> = baseline.iter().map(|c| c.kind).collect();
    kinds.sort_by_key(|k| k.as_str());
    let mut all_kinds = vec![
        ContractKind::Env,
        ContractKind::Http,
        ContractKind::Job,
        ContractKind::Queue,
        ContractKind::WebSocket,
    ];
    all_kinds.sort_by_key(|k| k.as_str());
    assert_eq!(kinds, all_kinds, "expected one contract of each kind");

    type OptionSetter = fn(&mut ContractOptions) -> &mut ContractOptions;
    let cases: [(ContractKind, OptionSetter); 5] = [
        (ContractKind::Http, |o| {
            o.http = false;
            o
        }),
        (ContractKind::Env, |o| {
            o.env = false;
            o
        }),
        (ContractKind::Queue, |o| {
            o.queue = false;
            o
        }),
        (ContractKind::WebSocket, |o| {
            o.websocket = false;
            o
        }),
        (ContractKind::Job, |o| {
            o.job = false;
            o
        }),
    ];
    for (kind, disable) in cases {
        let mut opts = ContractOptions::default();
        disable(&mut opts);
        let cands = extract_with(Lang::JavaScript, src, &opts);
        let gone: Vec<&ContractCandidate> =
            baseline.iter().filter(|c| !cands.contains(c)).collect();
        assert_eq!(gone.len(), 1, "{kind:?}: got {gone:?}");
        assert_eq!(gone[0].kind, kind, "{kind:?}: removed {gone:?}");
        // Survivors are byte-identical to the baseline entries.
        for c in &cands {
            assert!(baseline.contains(c), "{kind:?}: mutated {c:?}");
        }
    }

    // All kinds off: nothing at all.
    let all_off = ContractOptions {
        http: false,
        env: false,
        queue: false,
        websocket: false,
        job: false,
        grpc: false,
        graphql: false,
        openapi: false,
    };
    assert!(extract_with(Lang::JavaScript, src, &all_off).is_empty());
}

#[test]
fn acceptance_cross_kind_disambiguation() {
    // send on a websocket receiver is websocket, never queue.
    let ws = extract(Lang::JavaScript, "socket.send('hello');");
    assert_eq!(ws[0].kind, ContractKind::WebSocket);
    // A path-like literal on an unknown receiver is the ambiguous HTTP
    // tier, never queue.
    let http = extract(Lang::JavaScript, "registry.get('/users');");
    assert_eq!(http.len(), 1);
    assert_eq!(http[0].kind, ContractKind::Http);
    assert_eq!(http[0].confidence, 0.5);
}

// -- grpc generated/server code (TASK-088, plan 5.1) ------------------------

#[test]
fn grpc_java_impl_base_provider() {
    let src = "\
public class UserServiceImpl extends UserServiceGrpc.UserServiceImplBase {
    @Override
    public void getUser(GetUserRequest req, StreamObserver<User> obs) { }
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "grpc::UserService::getUser");
    assert_eq!(c.kind, ContractKind::Grpc);
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(c.owning_symbol.as_deref(), Some("getUser"));
    assert_eq!(c.line, 3);
}

#[test]
fn grpc_java_stub_consumers_bound_and_inline() {
    let src = "\
class Client {
    void call(Channel channel) {
        UserServiceGrpc.UserServiceBlockingStub stub = UserServiceGrpc.newBlockingStub(channel);
        stub.getUser(request);
        UserServiceGrpc.newBlockingStub(channel).getUser(request);
    }
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    for c in &cands {
        assert_eq!(c.canonical_id, "grpc::UserService::getUser");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.owning_symbol.as_deref(), Some("call"));
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }
    assert_eq!(cands[0].line, 4);
    assert_eq!(cands[1].line, 5);
}

#[test]
fn grpc_java_add_service_bind_service() {
    let src = "\
class Server {
    void start() {
        ServerBuilder.forPort(50051)
            .addService(UserServiceGrpc.bindService(new UserServiceImpl()))
            .build();
    }
}
";
    let cands = extract(Lang::Java, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "grpc::UserService::*");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("start"));
}

#[test]
fn grpc_go_register_server_provider() {
    let src = "\
package main

func serve() {
    s := grpc.NewServer()
    pb.RegisterUserServiceServer(s, &server{})
}
";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "grpc::UserService::*");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("serve"));
}

#[test]
fn grpc_go_client_consumers_bound_and_inline() {
    let src = "\
package main

func call(conn *grpc.ClientConn) error {
    client := pb.NewUserServiceClient(conn)
    _, err := client.GetUser(ctx, req)
    _, err2 := pb.NewUserServiceClient(conn).GetUser(ctx, req)
    return err
}
";
    let cands = extract(Lang::Go, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    for c in &cands {
        assert_eq!(c.canonical_id, "grpc::UserService::GetUser");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.owning_symbol.as_deref(), Some("call"));
    }
}

#[test]
fn grpc_rust_tonic_impl_provider_snake_case() {
    let src = "\
use tonic::{Request, Response, Status};

impl user_service_server::UserService for MyService {
    async fn get_user(&self, request: Request<GetUserRequest>)
        -> Result<Response<User>, Status> {
        todo!()
    }
}
";
    let cands = extract(Lang::Rust, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    // snake_case preserved in the ID; the canonical join folds casing.
    assert_eq!(c.canonical_id, "grpc::UserService::get_user");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("get_user"));
    assert_eq!(c.line, 4);
}

#[test]
fn grpc_rust_bound_client_await_consumer() {
    let src = "\
async fn call(channel: Channel) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = UserServiceClient::new(channel);
    let response = client.get_user(request).await?;
    Ok(())
}
";
    let cands = extract(Lang::Rust, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "grpc::UserService::get_user");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.owning_symbol.as_deref(), Some("call"));
}

#[test]
fn grpc_python_servicer_and_add_to_server() {
    let src = "\
import grpc
import user_service_pb2

class UserServiceServicer(user_service_pb2.UserServiceServicer):
    def GetUser(self, request, context):
        return user_service_pb2.User()

def serve():
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))
    add_UserServiceServicer_to_server(UserServiceServicer(), server)
";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let method = find(&cands, "grpc::UserService::GetUser").expect("method provider missing");
    assert_eq!(method.role, ContractRole::Provider);
    assert_eq!(method.owning_symbol.as_deref(), Some("GetUser"));
    assert_eq!(method.line, 5);
    let service = find(&cands, "grpc::UserService::*").expect("service provider missing");
    assert_eq!(service.role, ContractRole::Provider);
    assert_eq!(service.owning_symbol.as_deref(), Some("serve"));
}

#[test]
fn grpc_python_bound_stub_consumer() {
    let src = "\
import user_service_pb2

def call(channel):
    stub = user_service_pb2.UserServiceStub(channel)
    return stub.GetUser(user_service_pb2.GetUserRequest())
";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "grpc::UserService::GetUser");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.owning_symbol.as_deref(), Some("call"));
}

#[test]
fn grpc_js_gated_client_and_add_service() {
    let src = "\
const grpc = require('@grpc/grpc-js');
const client = new user.UserServiceClient(host, creds);
client.getUser(arg, cb);
server.addService(user.UserService.service, { getUser: handler });
";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let call = find(&cands, "grpc::user.UserService::getUser").expect("consumer missing");
    assert_eq!(call.role, ContractRole::Consumer);
    assert_eq!(call.confidence, CONFIDENCE_FRAMEWORK);
    // Package qualification preserved in the ID (plan 4); the canonical
    // join relaxes it at match time.
    let svc = find(&cands, "grpc::user.UserService::*").expect("service provider missing");
    assert_eq!(svc.role, ContractRole::Provider);
}

#[test]
fn grpc_js_negative_without_grpc_marker() {
    // new XClient alone is not evidence: with no grpc marker in the file
    // nothing is detected.
    let src = "\
const client = new user.UserServiceClient(host, creds);
client.getUser(arg, cb);
server.addService(user.UserService.service, { getUser: handler });
";
    let cands = extract(Lang::JavaScript, src);
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn grpc_generated_code_disabled_by_option() {
    let opts = ContractOptions {
        grpc: false,
        ..ContractOptions::default()
    };
    let src = "public class UserServiceImpl extends UserServiceGrpc.UserServiceImplBase {\n    public void getUser(GetUserRequest req, StreamObserver<User> obs) { }\n}\n";
    assert!(extract_with(Lang::Java, src, &opts).is_empty());
}
