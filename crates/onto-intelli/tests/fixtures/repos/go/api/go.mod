module example.com/mono/api

go 1.22

require (
	example.com/mono/lib v0.0.0-00010101000000-000000000000
	github.com/google/uuid v1.6.0 // indirect
)

replace example.com/mono/lib => ../lib
