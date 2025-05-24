import gleam/json
import gleam/io

pub type Pod {
  Pod(
    name: String,
    image: String,
    replicas: Int,
    ports: List(Int),
  )
}

pub fn pod_to_json(pod: Pod) -> json.Value {
  json.object([
    tuple("name", json.string(pod.name)),
    tuple("image", json.string(pod.image)),
    tuple("replicas", json.int(pod.replicas)),
    tuple("ports", json.array(pod.ports, json.int)),
  ])
}

pub fn main() {
  let pod = Pod("web", "nginx:1.25", 3, [80, 443])
  let encoded = pod_to_json(pod)
  let json_string = json.to_string(encoded)

  io.println(json_string)
}
