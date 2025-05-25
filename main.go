package main

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"time"

	"rezn/reznstore"
)

type GenericItem struct {
	Kind    string          `json:"kind"`
	Name    string          `json:"name"`
	Fields  json.RawMessage `json:"fields,omitempty"`
	Options []string        `json:"options,omitempty"`
}

type PodFields struct {
	Image    string `json:"image"`
	Replicas int    `json:"replicas"`
	Ports    []int  `json:"ports"`
	Secure   bool   `json:"secure"`
}

type PodSpec struct {
	Name     string `json:"name"`
	Image    string `json:"image"`
	Replicas int    `json:"replicas"`
	Ports    []int  `json:"ports"`
}

func main() {
	dbPath := "/var/lib/rezn"
	if len(os.Args) == 2 {
		dbPath = os.Args[1]
	}

	store, err := reznstore.New(dbPath)
	if err != nil {
		fmt.Printf("Failed to open store: %v\n", err)
		os.Exit(1)
	}
	defer store.Close()

	for {
		err := reconcileLoop(store)
		if err != nil {
			fmt.Printf("Error: %v\n", err)
		}
		time.Sleep(5 * time.Second)
	}
}

func reconcileLoop(store *reznstore.Store) error {
	data, err := store.Read("desired")
	if err != nil {
		return fmt.Errorf("failed to read desired state: %w", err)
	}

	var items []GenericItem
	if err := json.Unmarshal(data, &items); err != nil {
		return fmt.Errorf("failed to unmarshal desired state: %w", err)
	}

	var desiredPods []PodSpec
	for _, item := range items {
		if item.Kind == "pod" {
			var fields PodFields
			if err := json.Unmarshal(item.Fields, &fields); err != nil {
				fmt.Printf("bad pod %s: %v\n", item.Name, err)
				continue
			}
			desiredPods = append(desiredPods, PodSpec{
				Name:     item.Name,
				Image:    fields.Image,
				Replicas: fields.Replicas,
				Ports:    fields.Ports,
			})
		}
	}

	running, err := listRunningContainers()
	if err != nil {
		return err
	}

	for _, pod := range desiredPods {
		var matches []string
		for _, c := range running {
			if strings.HasPrefix(c, pod.Name+"-") {
				matches = append(matches, c)
			}
		}

		if len(matches) < pod.Replicas {
			missing := pod.Replicas - len(matches)
			for i := 0; i < missing; i++ {
				containerName := fmt.Sprintf("%s-%d", pod.Name, time.Now().UnixNano())
				err := startContainer(containerName, pod)
				if err != nil {
					fmt.Printf("Failed to start container %s: %v\n", containerName, err)
				}
			}
		} else if len(matches) > pod.Replicas {
			extra := len(matches) - pod.Replicas
			for i := 0; i < extra; i++ {
				if err := stopContainer(matches[i]); err != nil {
					fmt.Printf("Failed to stop container %s: %v\n", matches[i], err)
				}
			}
		}
	}

	return nil
}

func listRunningContainers() ([]string, error) {
	cmd := exec.Command("docker", "ps", "--format", "{{.Names}}")
	output, err := cmd.Output()
	if err != nil {
		return nil, err
	}
	lines := strings.Split(strings.TrimSpace(string(output)), "\n")
	if len(lines) == 1 && lines[0] == "" {
		return []string{}, nil
	}
	return lines, nil
}

func startContainer(name string, pod PodSpec) error {
	args := []string{"run", "-d", "--name", name}
	for _, port := range pod.Ports {
		args = append(args, "-p", fmt.Sprintf("%d:%d", port, port))
	}
	args = append(args, pod.Image)
	cmd := exec.Command("docker", args...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func stopContainer(name string) error {
	cmd := exec.Command("docker", "rm", "-f", name)
	return cmd.Run()
}
