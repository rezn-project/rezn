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

pub fn pod_to_json(pod: Pod) -> json.Json {
  json.object([
    #("name", json.string(pod.name)),
    #("image", json.string(pod.image)),
    #("replicas", json.int(pod.replicas)),
    #("ports", json.array(pod.ports, json.int)),
  ])
}

pub fn pod_list_to_json(pods: List(Pod)) -> json.Json {
  json.array(pods, pod_to_json)
}

pub fn main() {
  let pods = [
    Pod("web", "nginx:1.25", 3, [80]),
    Pod("api", "my-api:latest", 2, [4000])
  ]

  let json_string = pod_list_to_json(pods)
  |> json.to_string

  io.println(json_string)
}
