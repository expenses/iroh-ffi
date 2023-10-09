package main

import (
	"fmt"
	"os"

	"github.com/n0-computer/iroh-ffi/iroh"
)

func main() {
	fmt.Printf("Booting...\n")
	nodeDir := "./iroh-node-go"
	if err := os.Mkdir(nodeDir, os.ModePerm); err != nil {
		panic(err)
	}
	node, err := iroh.NewIrohNode(nodeDir)
	if err != nil {
		panic(err)
	}

	nodeID := node.NodeId()
	fmt.Printf("Hello, iroh %s from go!\n", nodeID)

	conns, err := node.Connections()
	if err != nil {
		panic(err)
	}

	fmt.Printf("Got %d connections\n", len(conns))
	for _, conn := range conns {
		fmt.Printf("conn: %v\n", conn)
	}

	doc, err := node.DocNew()
	if err != nil {
		panic(err)
	}
	fmt.Printf("Created document %s\n", doc.Id())
	author, err := node.AuthorNew()
	if err != nil {
		panic(err)
	}
	fmt.Printf("Created author %s\n", author.ToString())
	hash, err := doc.SetBytes(author, []byte("go"), []byte("says hello"))
	if err != nil {
		panic(err)
	}
	fmt.Printf("Inserted %s\n", hash.ToString())

	// content, err := doc.GetContentBytes(entry)
	// if err != nil {
	// 	panic(err)
	// }
	// fmt.Printf("Got content \"%s\"\n", string(content))

	hash, err = doc.SetBytes(author, []byte("another one"), []byte("says hello"))
	if err != nil {
		panic(err)
	}
	fmt.Printf("Inserted %s\n", hash.ToString())

	entries, err := doc.Keys()
	if err != nil {
		panic(err)
	}
	fmt.Printf("Got %d entries\n", len(entries))
	for _, entry := range entries {
		content, err := doc.GetContentBytes(entry)
		if err != nil {
			panic(err)
		}
		fmt.Printf("Entry: %s: \"%s\"\n", string(entry.Key()), string(content))
	}

	fmt.Printf("Goodbye!\n")
}