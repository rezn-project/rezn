import gleam/io
import gleam/json

pub type Pod {
  Pod(
    name: String,
    image: String,
    replicas: Int,
    ports: List(Int),
  )
}

pub fn pod_to_json(pod: Pod) -> String {
  json.object([
    #("name", json.string(pod.name)),
    #("image", json.string(pod.image)),
    #("replicas", json.int(pod.replicas)),
    #("ports", json.array(pod.ports, json.int)),
  ])
  |> json.to_string
}

pub fn main() {
  let pod = Pod("web", "nginx:1.25", 3, [80, 443])
  let json_string = pod_to_json(pod)
  io.println(json_string)
}
